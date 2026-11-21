package com.amurcanov.tgwsproxy

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.os.SystemClock
import android.provider.Settings
import android.text.format.Formatter
import android.util.Log
import android.widget.Toast
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlin.time.Duration.Companion.milliseconds
import java.util.concurrent.Executors

class ProxyService : Service() {

    private val connectivity by lazy { getSystemService(CONNECTIVITY_SERVICE) as ConnectivityManager }
    private var defaultNetwork: Network? = null
    private var defaultTransport = -1
    private var defaultLinks: Pair<List<String>, List<String>>? = null
    private var observesDefaultNetwork = false
    private val networkHandler = Handler(Looper.getMainLooper())
    private val networkSnapshot = Runnable { reportDefaultNetwork() }
    private val bootCount by lazy {
        runCatching { Settings.Global.getInt(contentResolver, "boot_count", -1) }.getOrDefault(-1)
    }
    private val defaultNetworkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) = scheduleNetworkSnapshot()
        override fun onLost(network: Network) = scheduleNetworkSnapshot()
        override fun onCapabilitiesChanged(network: Network, capabilities: NetworkCapabilities) =
            scheduleNetworkSnapshot()

        override fun onLinkPropertiesChanged(network: Network, linkProperties: LinkProperties) =
            scheduleNetworkSnapshot()
    }

    private var wakeLock: PowerManager.WakeLock? = null
    private var statsJob: Job? = null
    private var restartJob: Job? = null
    private var lastNotificationContent: String = ""
    private var lastNotificationDetails: String? = null
    private var inForeground = false
    private var sessionStartedAtMs: Long = 0L
    @Volatile
    private var stopInProgress = false
    @Volatile
    private var destroyed = false
    @Volatile
    private var lifecycleGeneration = 0L
    private val serviceScope = CoroutineScope(Dispatchers.IO + SupervisorJob())

    // Saved intent extras for restart on kill / onTaskRemoved
    private var lastBindIp: String = "127.0.0.1"
    private var lastPort: Int = 1443
    private var lastIps: String = ""
    private var lastPoolSize: Int = 4
    private var lastBufferKb: Int = 256
    private var lastCfPlainWs80: Boolean = false
    private var lastWorkerDomains: String = ""
    private var lastCfEnabled: Boolean = true
    private var lastCfPriority: Boolean = true
    private var lastCfDomain: String = ""
    private var lastSecretKey: String = ""

    companion object {
        const val ACTION_START = "com.amurcanov.tgwsproxy.START"
        const val ACTION_STOP = "com.amurcanov.tgwsproxy.STOP"
        const val ACTION_RESTART = "com.amurcanov.tgwsproxy.RESTART"
        const val EXTRA_BIND_IP = "EXTRA_BIND_IP"
        const val EXTRA_PORT = "EXTRA_PORT"
        const val EXTRA_IPS = "EXTRA_IPS"
        const val EXTRA_POOL_SIZE = "EXTRA_POOL_SIZE"
        const val EXTRA_BUFFER_KB = "EXTRA_BUFFER_KB"
        const val EXTRA_CF_PLAIN_WS80 = "EXTRA_CF_PLAIN_WS80"
        const val EXTRA_WORKER_DOMAINS = "EXTRA_WORKER_DOMAINS"
        const val EXTRA_CFPROXY_ENABLED = "EXTRA_CFPROXY_ENABLED"
        const val EXTRA_CFPROXY_PRIORITY = "EXTRA_CFPROXY_PRIORITY"
        const val EXTRA_CFPROXY_DOMAIN = "EXTRA_CFPROXY_DOMAIN"
        const val EXTRA_SECRET_KEY = "EXTRA_SECRET_KEY"
        const val EXTRA_OPEN_WS80_SETTINGS = "EXTRA_OPEN_WS80_SETTINGS"
        
        private const val NOTIFICATION_ID = 101
        private const val WS80_SUGGESTION_NOTIFICATION_ID = 102
        private const val CHANNEL_ID = "TG_WS_Proxy_Service_v4"
        private const val SUGGESTION_CHANNEL_ID = "TG_WS_Proxy_Suggestions_v1"
        private const val TAG = "ProxyService"

        // Wakelock refresh interval (25 min, re-acquire before 30-min timeout)
        private const val WAKELOCK_TIMEOUT_MS = 30L * 60 * 1000
        private const val WAKELOCK_REFRESH_MS = 25L * 60 * 1000

        // Stats/notification update interval
        private const val STATS_UPDATE_MS = 3_000L
        private const val OUTAGE_CONFIRMATION_MS = 9_000L
        private const val NATIVE_STOP_WAIT_MS = 3_000L
        private const val NETWORK_SNAPSHOT_DELAY_MS = 250L
        // Serialize native configuration/start/stop across service recreations.
        private val nativeExecutor = Executors.newSingleThreadExecutor { task ->
            Thread(task, "ProxyNative").apply { isDaemon = true }
        }

        private val _isRunning = MutableStateFlow(false)
        val isRunning: StateFlow<Boolean> = _isRunning
        private val _isVerifiedRunning = MutableStateFlow(false)
        val isVerifiedRunning: StateFlow<Boolean> = _isVerifiedRunning
        private val _connectionState = MutableStateFlow(ProxyConnectionState.STOPPED)
        val connectionState: StateFlow<ProxyConnectionState> = _connectionState
        private val _startupInProgress = MutableStateFlow(false)
        val startupInProgress: StateFlow<Boolean> = _startupInProgress
        private val _startupError = MutableStateFlow<String?>(null)
        val startupError: StateFlow<String?> = _startupError
        private val _sessionTraffic = MutableStateFlow<ProxySessionTraffic?>(null)
        internal val sessionTraffic: StateFlow<ProxySessionTraffic?> = _sessionTraffic
        private val _ws80SuggestionActive = MutableStateFlow(false)
        val ws80SuggestionActive: StateFlow<Boolean> = _ws80SuggestionActive
        @Volatile
        private var ws80LastGeneration: Long = 0L
        @Volatile
        private var ws80AcknowledgedGeneration: Long = 0L

        fun clearWs80Suggestion(context: android.content.Context, acknowledgeCurrentOutage: Boolean = true) {
            if (acknowledgeCurrentOutage && ws80LastGeneration > 0L) {
                ws80AcknowledgedGeneration = ws80LastGeneration
            }
            _ws80SuggestionActive.value = false
            val manager = context.getSystemService(NOTIFICATION_SERVICE) as? NotificationManager
            manager?.cancel(WS80_SUGGESTION_NOTIFICATION_ID)
        }
    }

    override fun onCreate() {
        super.onCreate()
        createNotificationChannel()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
            try {
                connectivity.registerDefaultNetworkCallback(defaultNetworkCallback)
                observesDefaultNetwork = true
            } catch (e: RuntimeException) {
                Log.w(TAG, "Default-network observer unavailable", e)
            }
        }
        reportDefaultNetwork()
    }

    private fun scheduleNetworkSnapshot() {
        networkHandler.removeCallbacks(networkSnapshot)
        networkHandler.postDelayed(networkSnapshot, NETWORK_SNAPSHOT_DELAY_MS)
    }

    private fun reportDefaultNetwork() {
        if (destroyed || Build.VERSION.SDK_INT < Build.VERSION_CODES.M) return
        val active = connectivity.activeNetwork
        val capabilities = connectivity.getNetworkCapabilities(active)
        val transport = when {
            active == null -> 0
            capabilities?.hasTransport(NetworkCapabilities.TRANSPORT_VPN) == true -> 4
            capabilities?.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) == true -> 1
            capabilities?.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) == true -> 2
            else -> 3
        }
        val links = connectivity.getLinkProperties(active)?.let {
            it.linkAddresses.map { address -> address.toString() }.sorted() to
                it.dnsServers.map { address -> address.hostAddress.orEmpty() }
        }
        if (active == defaultNetwork && transport == defaultTransport && links == defaultLinks) return
        val replaced = active != defaultNetwork
        defaultNetwork = active
        defaultTransport = transport
        defaultLinks = links
        val identity = if (active != null && bootCount >= 0 && transport in 1..2) {
            active.networkHandle xor (bootCount.toLong() shl 17)
        } else 0L
        NativeProxy.notifyDefaultNetwork(transport, identity, replaced)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_START -> {
                if (BuildConfig.DEBUG) LogManager.clearLogs()
                val bindIp = intent.getStringExtra(EXTRA_BIND_IP) ?: "127.0.0.1"
                val port = intent.getIntExtra(EXTRA_PORT, 1443)
                val ips = intent.getStringExtra(EXTRA_IPS) ?: ""
                val poolSize = intent.getIntExtra(EXTRA_POOL_SIZE, 4)
                val bufferKb = intent.getIntExtra(EXTRA_BUFFER_KB, 256)
                val cfPlainWs80 = intent.getBooleanExtra(EXTRA_CF_PLAIN_WS80, false)
                val workerDomains = intent.getStringExtra(EXTRA_WORKER_DOMAINS) ?: ""
                val cfEnabled = intent.getBooleanExtra(EXTRA_CFPROXY_ENABLED, true)
                val cfPriority = intent.getBooleanExtra(EXTRA_CFPROXY_PRIORITY, true)
                val cfDomain = intent.getStringExtra(EXTRA_CFPROXY_DOMAIN) ?: ""
                val secretKey = intent.getStringExtra(EXTRA_SECRET_KEY) ?: ""
                startProxy(bindIp, port, ips, poolSize, cfEnabled, cfPriority, cfDomain, secretKey, bufferKb, workerDomains, cfPlainWs80)
            }
            ACTION_STOP -> {
                stopProxy()
            }
            ACTION_RESTART -> {
                restartProxy()
            }
            null -> {
                // Service restarted by system after being killed (START_REDELIVER_INTENT)
                // If we had saved params, try to restart
                if (lastPort > 0 && lastSecretKey.isNotEmpty()) {
                    Log.w(TAG, "Service restarted by system, re-starting proxy")
                    startProxy(lastBindIp, lastPort, lastIps, lastPoolSize, lastCfEnabled, lastCfPriority, lastCfDomain, lastSecretKey, lastBufferKb, lastWorkerDomains, lastCfPlainWs80)
                } else {
                    stopSelf()
                }
            }
        }
        // START_REDELIVER_INTENT: if the system kills the service, it will restart it
        // and re-deliver the last intent, so we don't lose the config.
        return START_REDELIVER_INTENT
    }

    private fun startProxy(bindIp: String, port: Int, ips: String, poolSize: Int = 4,
                           cfEnabled: Boolean = true, cfPriority: Boolean = true,
                           cfDomain: String = "", secretKey: String = "",
                           bufferKb: Int = 256, workerDomains: String = "",
                           cfPlainWs80: Boolean = false) {
        if (destroyed || _isRunning.value || _startupInProgress.value ||
            stopInProgress || restartJob?.isActive == true) return
        val generation = ++lifecycleGeneration
        _isVerifiedRunning.value = false
        _sessionTraffic.value = null
        _startupError.value = null
        _startupInProgress.value = true

        // Save params for restart
        lastBindIp = bindIp
        lastPort = port
        lastIps = ips
        lastPoolSize = poolSize
        lastBufferKb = bufferKb
        lastCfPlainWs80 = cfPlainWs80
        lastWorkerDomains = workerDomains
        lastCfEnabled = cfEnabled
        lastCfPriority = cfPriority
        lastCfDomain = cfDomain
        lastSecretKey = secretKey
        sessionStartedAtMs = SystemClock.elapsedRealtime()
        lastNotificationDetails = null
        lastNotificationContent = getString(R.string.notification_starting)

        val notification = createNotification(lastNotificationContent)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
        inForeground = true

        acquireWakeLock()
        stopInProgress = false
        
        // Set the native options before the listener starts accepting clients.
        nativeExecutor.execute {
            if (generation != lifecycleGeneration || destroyed) return@execute
            try {
                NativeProxy.setPoolSize(poolSize)
                NativeProxy.setBufferSizeKb(bufferKb)
                NativeProxy.setCfPlainWs80(cfPlainWs80)
                NativeProxy.setCfWorkerDomains(workerDomains)
                NativeProxy.setCfProxyCacheDir(cacheDir.absolutePath)
                NativeProxy.setRouteCacheDir(noBackupFilesDir.absolutePath)
                NativeProxy.setStickyRouting(runBlocking { SettingsStore(applicationContext).stickyRouting.first() })
                NativeProxy.setCfProxyConfig(cfEnabled, cfPriority, cfDomain)
                val result = NativeProxy.startProxy(bindIp, port, ips, secretKey, if (BuildConfig.DEBUG) 1 else 0)
                if (result == 0) {
                    serviceScope.launch(Dispatchers.Main) {
                        if (generation == lifecycleGeneration && !destroyed) {
                            updateRunningState(true)
                            _isVerifiedRunning.value = true
                            _connectionState.value = ProxyConnectionState.LISTENING
                            _startupInProgress.value = false
                            updateNotification(getString(R.string.status_listening), force = true)
                            Log.i(TAG, "Proxy ready: listening on port $port")
                        }
                    }
                } else {
                    Log.e(TAG, "StartProxy returned error code: $result")
                    serviceScope.launch(Dispatchers.Main) {
                        if (generation != lifecycleGeneration || destroyed) return@launch
                        val message = if (result == -4) {
                            getString(R.string.port_is_not_available, port).also {
                                Toast.makeText(this@ProxyService, it, Toast.LENGTH_SHORT).show()
                            }
                        } else {
                            getString(R.string.notification_start_error_code, result)
                        }
                        _startupError.value = message
                        _startupInProgress.value = false
                        updateNotification(message, force = true)
                        delay(3000.milliseconds)
                        if (generation == lifecycleGeneration) stopProxy()
                    }
                }
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to start proxy via JNA", e)
                serviceScope.launch(Dispatchers.Main) {
                    if (generation != lifecycleGeneration || destroyed) return@launch
                    val message = getString(R.string.notification_start_failed)
                    _startupError.value = message
                    _startupInProgress.value = false
                    updateNotification(message, force = true)
                    delay(3000.milliseconds)
                    if (generation == lifecycleGeneration) stopProxy()
                }
            }
        }

        // Stats updater. Notification updates are throttled so the system keeps
        // a stable foreground entry instead of constantly reordering it.
        statsJob = serviceScope.launch {
            var outageSince: Long? = null
            // WakeLock refresh sub-job: re-acquire before system timeout
            launch {
                while (isActive) {
                    delay(WAKELOCK_REFRESH_MS.milliseconds)
                    refreshWakeLock()
                }
            }

            while (isActive) {
                delay(STATS_UPDATE_MS.milliseconds)
                if (_isRunning.value && !stopInProgress) {
                    try {
                        val observation = NativeProxy.connectionState()
                        if (!isActive || generation != lifecycleGeneration) return@launch
                        if (observation == ProxyConnectionState.STOPPED) {
                            withContext(Dispatchers.Main) {
                                if (generation == lifecycleGeneration && !stopInProgress) {
                                    _startupError.value = getString(R.string.notification_start_failed)
                                    stopProxy()
                                }
                            }
                            return@launch
                        }
                        val now = SystemClock.elapsedRealtime()
                        val state = if (observation == ProxyConnectionState.NO_AVAILABLE_ROUTE) {
                            val since = outageSince ?: now.also { outageSince = it }
                            if (now - since >= OUTAGE_CONFIRMATION_MS) observation else ProxyConnectionState.CONNECTING
                        } else {
                            outageSince = null
                            observation
                        }
                        val traffic = ProxySessionTraffic.parse(NativeProxy.getStats())
                        val suggestionSnapshot = Ws80SuggestionSnapshot.parse(NativeProxy.routeDiagnostics())
                        if (suggestionSnapshot != null && suggestionSnapshot.generation > 0L) {
                            ws80LastGeneration = suggestionSnapshot.generation
                        }
                        val store = SettingsStore(applicationContext)
                        val savedWs80 = store.cfPlainWs80.first()
                        val dismissedUntil = store.ws80SuggestionDismissedUntil.first()
                        val neverSuggest = store.ws80SuggestionNever.first()
                        val suggestWs80 = Ws80SuggestionPolicy.shouldSuggest(
                            isRunning = _isRunning.value && !stopInProgress,
                            snapshot = suggestionSnapshot,
                            savedCfPlainWs80 = savedWs80,
                            dismissedUntilMs = dismissedUntil,
                            neverSuggest = neverSuggest,
                            acknowledgedGeneration = ws80AcknowledgedGeneration,
                            nowMs = System.currentTimeMillis(),
                        )
                        withContext(Dispatchers.Main.immediate) {
                            if (generation == lifecycleGeneration && inForeground && !stopInProgress) {
                                _connectionState.value = state
                                traffic?.let { _sessionTraffic.value = it }
                                updateSessionNotification(state)
                                updateWs80SuggestionNotification(suggestWs80)
                            }
                        }
                    } catch (e: Exception) {
                        Log.w(TAG, "Stats update failed", e)
                    }
                }
            }
        }
    }

    private fun updateSessionNotification(state: ProxyConnectionState) {
        val traffic = _sessionTraffic.value
        val status = getString(when (state) {
            ProxyConnectionState.LISTENING -> R.string.notification_waiting_telegram
            ProxyConnectionState.CONNECTING, ProxyConnectionState.NO_AVAILABLE_ROUTE -> state.label
            else -> R.string.notification_running
        })
        val total = traffic?.let { Formatter.formatFileSize(this, it.total) }
        val content = if (total == null) status else getString(R.string.notification_session, status, total)
        val details = if (traffic != null && total != null) {
            getString(
                R.string.notification_session_details,
                status,
                total,
                Formatter.formatFileSize(this, traffic.received),
                Formatter.formatFileSize(this, traffic.sent)
            )
        } else null
        updateNotification(content, details)
    }

    private fun updateNotification(content: String, details: String? = null, force: Boolean = false) {
        if (!force && content == lastNotificationContent && details == lastNotificationDetails) return

        lastNotificationContent = content
        lastNotificationDetails = details
        try {
            val manager = getSystemService(NOTIFICATION_SERVICE) as? NotificationManager
            manager?.notify(NOTIFICATION_ID, createNotification(content, details))
        } catch (e: Exception) {
            Log.w(TAG, "Failed to update notification", e)
        }
    }

    private fun restartProxy() {
        if (stopInProgress || restartJob?.isActive == true) return
        if (lastPort <= 0 || lastSecretKey.isEmpty()) {
            Log.w(TAG, "Restart requested without saved proxy configuration")
            return
        }

        ++lifecycleGeneration
        _startupInProgress.value = false
        restartJob = serviceScope.launch(Dispatchers.Main.immediate) {
            Log.i(TAG, "Restarting proxy from notification")
            updateNotification(getString(R.string.notification_restarting), force = true)

            statsJob?.cancel()
            statsJob = null

            requestNativeStop("restart")
            releaseWakeLock()
            updateRunningState(false)
            restartJob = null
            startProxy(
                bindIp = lastBindIp,
                port = lastPort,
                ips = lastIps,
                poolSize = lastPoolSize,
                cfEnabled = lastCfEnabled,
                bufferKb = lastBufferKb,
                cfPlainWs80 = lastCfPlainWs80,
                workerDomains = lastWorkerDomains,
                cfPriority = lastCfPriority,
                cfDomain = lastCfDomain,
                secretKey = lastSecretKey
            )
        }
    }

    private fun stopProxy() {
        if (stopInProgress) return
        stopInProgress = true
        ++lifecycleGeneration
        restartJob?.cancel()
        restartJob = null
        statsJob?.cancel()
        statsJob = null
        serviceScope.launch(Dispatchers.Main.immediate) {
            updateNotification(getString(R.string.notification_stopping), force = true)
            requestNativeStop("stop")
            releaseWakeLock()
            updateRunningState(false)
            inForeground = false
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
                stopForeground(STOP_FOREGROUND_REMOVE)
            } else {
                @Suppress("DEPRECATION")
                stopForeground(true)
            }
            stopSelf()
        }
    }

    private fun queueNativeStop(reason: String): Deferred<Unit> {
        val completed = CompletableDeferred<Unit>()
        nativeExecutor.execute {
            try {
                NativeProxy.stopProxy()
            } catch (e: Exception) {
                Log.w(TAG, "StopProxy failed during $reason", e)
            } catch (e: LinkageError) {
                Log.w(TAG, "Native proxy library unavailable during $reason", e)
            } finally {
                completed.complete(Unit)
            }
        }
        return completed
    }

    private suspend fun requestNativeStop(reason: String): Boolean {
        val completed = queueNativeStop(reason)
        val finished = withTimeoutOrNull(NATIVE_STOP_WAIT_MS.milliseconds) {
            completed.await()
            true
        } ?: false

        if (!finished) {
            Log.w(TAG, "Native stop is still running after ${NATIVE_STOP_WAIT_MS}ms during $reason")
        }
        return finished
    }

    /**
     * Called when the user swipes the app from recents.
     * Without this, the service would be killed on many OEM Androids.
     */
    override fun onTaskRemoved(rootIntent: Intent?) {
        super.onTaskRemoved(rootIntent)
        if (_isRunning.value) {
            Log.w(TAG, "onTaskRemoved: proxy is running, service stays alive")
            // The service continues because stopWithTask=false in manifest
            // No action needed — the service keeps running.
        }
    }

    private fun acquireWakeLock() {
        try {
            val powerManager = getSystemService(POWER_SERVICE) as PowerManager
            wakeLock = powerManager.newWakeLock(
                PowerManager.PARTIAL_WAKE_LOCK,
                "TgWsProxy::ServiceWakeLock"
            ).apply {
                // Acquire with timeout. System may ignore indefinite wakelocks.
                acquire(WAKELOCK_TIMEOUT_MS)
            }
            Log.d(TAG, "WakeLock acquired (${WAKELOCK_TIMEOUT_MS / 60000}min)")
        } catch (e: Exception) {
            Log.w(TAG, "Failed to acquire WakeLock", e)
        }
    }

    /**
     * Periodically refresh wakelock to prevent system from expiring it.
     */
    private fun refreshWakeLock() {
        try {
            wakeLock?.let {
                if (it.isHeld) {
                    it.release()
                }
            }
            val powerManager = getSystemService(POWER_SERVICE) as PowerManager
            wakeLock = powerManager.newWakeLock(
                PowerManager.PARTIAL_WAKE_LOCK,
                "TgWsProxy::ServiceWakeLock"
            ).apply {
                acquire(WAKELOCK_TIMEOUT_MS)
            }
            Log.d(TAG, "WakeLock refreshed")
        } catch (e: Exception) {
            Log.w(TAG, "Failed to refresh WakeLock", e)
        }
    }

    private fun releaseWakeLock() {
        try {
            wakeLock?.let {
                if (it.isHeld) {
                    it.release()
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "Failed to release WakeLock", e)
        }
        wakeLock = null
    }

    private fun updateRunningState(isRunning: Boolean) {
        _isRunning.value = isRunning
        if (!isRunning) {
            _sessionTraffic.value = null
            _startupInProgress.value = false
            _isVerifiedRunning.value = false
            _connectionState.value = ProxyConnectionState.STOPPED
            clearWs80Suggestion(this, acknowledgeCurrentOutage = false)
            ws80AcknowledgedGeneration = 0L
        }
        ProxyTileService.requestSync(this)
    }

    private fun updateWs80SuggestionNotification(active: Boolean) {
        val wasActive = _ws80SuggestionActive.value
        _ws80SuggestionActive.value = active
        val manager = getSystemService(NOTIFICATION_SERVICE) as? NotificationManager ?: return
        if (!active) {
            if (wasActive) manager.cancel(WS80_SUGGESTION_NOTIFICATION_ID)
            return
        }
        if (wasActive) return
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(this, android.Manifest.permission.POST_NOTIFICATIONS) !=
            android.content.pm.PackageManager.PERMISSION_GRANTED
        ) {
            return
        }
        try {
            val openIntent = Intent(this, MainActivity::class.java).apply {
                flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
                putExtra(EXTRA_OPEN_WS80_SETTINGS, true)
            }
            val pendingIntent = PendingIntent.getActivity(
                this, 202, openIntent,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
            )
            val notification = NotificationCompat.Builder(this, SUGGESTION_CHANNEL_ID)
                .setSmallIcon(R.drawable.ic_notification_relay)
                .setContentTitle(getString(R.string.ws80_suggest_title))
                .setContentText(getString(R.string.ws80_suggest_message))
                .setStyle(NotificationCompat.BigTextStyle().bigText(getString(R.string.ws80_suggest_message)))
                .setAutoCancel(true)
                .setOnlyAlertOnce(true)
                .setContentIntent(pendingIntent)
                .addAction(0, getString(R.string.ws80_suggest_open_settings), pendingIntent)
                .setPriority(NotificationCompat.PRIORITY_LOW)
                .build()
            manager.notify(WS80_SUGGESTION_NOTIFICATION_ID, notification)
        } catch (e: Exception) {
            Log.w(TAG, "Failed to post WS/80 suggestion notification", e)
        }
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val serviceChannel = NotificationChannel(
                CHANNEL_ID,
                getString(R.string.notification_channel_name),
                NotificationManager.IMPORTANCE_LOW
            ).apply {
                description = getString(R.string.notification_channel_description)
                setShowBadge(false)
                setSound(null, null)
                enableVibration(false)
                enableLights(false)
                lockscreenVisibility = Notification.VISIBILITY_PUBLIC
            }
            val suggestionChannel = NotificationChannel(
                SUGGESTION_CHANNEL_ID,
                getString(R.string.ws80_suggest_channel_name),
                NotificationManager.IMPORTANCE_LOW
            ).apply {
                setShowBadge(false)
                setSound(null, null)
                enableVibration(false)
            }
            val manager = getSystemService(NOTIFICATION_SERVICE) as? NotificationManager
            manager?.createNotificationChannel(serviceChannel)
            manager?.createNotificationChannel(suggestionChannel)
        }
    }

    private fun createNotification(content: String, details: String? = null): Notification {
        val openIntent = Intent(this, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
        }
        val openPendingIntent = PendingIntent.getActivity(
            this, 1, openIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )

        val stopIntent = Intent(this, ProxyService::class.java).apply {
            action = ACTION_STOP
        }
        val stopPendingIntent = PendingIntent.getService(
            this, 0, stopIntent, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )

        val elapsed = (SystemClock.elapsedRealtime() - sessionStartedAtMs).coerceAtLeast(0L)
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(content)
            .apply { details?.let { setStyle(NotificationCompat.BigTextStyle().bigText(it)) } }
            .setSmallIcon(R.drawable.ic_notification_relay)
            .setContentIntent(openPendingIntent) // Tap notification → open app
            .addAction(android.R.drawable.ic_menu_view, getString(R.string.notification_open_app), openPendingIntent)
            .addAction(android.R.drawable.ic_menu_close_clear_cancel, getString(R.string.notification_disconnect), stopPendingIntent)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setSilent(true)
            .setVisibility(NotificationCompat.VISIBILITY_PUBLIC)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            // System UI ticks the clock even when the traffic totals are unchanged.
            .setWhen(System.currentTimeMillis() - elapsed)
            .setShowWhen(true)
            .setUsesChronometer(true)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .build()
    }

    override fun onDestroy() {
        destroyed = true
        ++lifecycleGeneration
        if (!stopInProgress) queueNativeStop("destroy")
        if (observesDefaultNetwork) {
            connectivity.unregisterNetworkCallback(defaultNetworkCallback)
            observesDefaultNetwork = false
        }
        networkHandler.removeCallbacks(networkSnapshot)
        restartJob?.cancel()
        restartJob = null
        statsJob?.cancel()
        statsJob = null
        releaseWakeLock()
        updateRunningState(false)
        serviceScope.cancel()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? {
        return null
    }
}
