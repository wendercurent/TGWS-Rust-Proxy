package com.amurcanov.tgwsproxy.ui

import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.text.format.Formatter
import android.view.HapticFeedbackConstants
import android.widget.Toast
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.selection.selectableGroup
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.ContentCopy
import androidx.compose.material.icons.filled.ExpandLess
import androidx.compose.material.icons.filled.ExpandMore
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.lifecycleScope
import com.amurcanov.tgwsproxy.BuildConfig
import com.amurcanov.tgwsproxy.NativeProxy
import com.amurcanov.tgwsproxy.ProxyController
import com.amurcanov.tgwsproxy.ProxyConnectionState
import com.amurcanov.tgwsproxy.ProxyService
import com.amurcanov.tgwsproxy.SettingsStore
import com.amurcanov.tgwsproxy.R
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import org.json.JSONObject

@Composable
fun ConnectionTab(
    settingsStore: SettingsStore,
    visible: Boolean,
    onOpenWs80Settings: () -> Unit = {}
) {
    val context = LocalContext.current
    val view = LocalView.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val isRunning by ProxyService.isRunning.collectAsStateWithLifecycle()
    val ready by ProxyService.isVerifiedRunning.collectAsStateWithLifecycle()
    val starting by ProxyService.startupInProgress.collectAsStateWithLifecycle()
    val startupError by ProxyService.startupError.collectAsStateWithLifecycle()
    val connectionState by ProxyService.connectionState.collectAsStateWithLifecycle()
    val ws80SuggestionActive by ProxyService.ws80SuggestionActive.collectAsStateWithLifecycle()
    val ws80Never by settingsStore.ws80SuggestionNever.collectAsStateWithLifecycle(initialValue = false)
    val ws80DismissedUntil by settingsStore.ws80SuggestionDismissedUntil.collectAsStateWithLifecycle(initialValue = 0L)
    val savedCfPlainWs80 by settingsStore.cfPlainWs80.collectAsStateWithLifecycle(initialValue = false)
    val isSettingsReady by settingsStore.isReady.collectAsStateWithLifecycle(initialValue = false)
    val savedPort by settingsStore.port.collectAsStateWithLifecycle(initialValue = "1443")
    val savedBindIp by settingsStore.bindIp.collectAsStateWithLifecycle(initialValue = "127.0.0.1")
    val cfEnabled by settingsStore.cfproxyEnabled.collectAsStateWithLifecycle(initialValue = true)
    val pool by settingsStore.poolSize.collectAsStateWithLifecycle(initialValue = 4)
    val savedSecretKey by settingsStore.secretKey.collectAsStateWithLifecycle(initialValue = "LOADING")

    if (!isSettingsReady || savedSecretKey == "LOADING") {
        Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
            CircularProgressIndicator(Modifier.size(24.dp), strokeWidth = 2.dp)
        }
        return
    }

    LaunchedEffect(savedSecretKey) {
        if (savedSecretKey.isEmpty()) {
            val bytes = ByteArray(16)
            java.security.SecureRandom().nextBytes(bytes)
            settingsStore.saveSecretKey(bytes.joinToString("") { "%02x".format(it) })
        }
    }

    var dispatching by remember { mutableStateOf(false) }
    var stopping by remember { mutableStateOf(false) }
    var dispatchError by remember { mutableStateOf<String?>(null) }
    var applyMode by rememberSaveable { mutableStateOf("packages") }
    val sessionTraffic by ProxyService.sessionTraffic.collectAsStateWithLifecycle()

    val active = dispatching || starting || isRunning
    val error = dispatchError ?: startupError
    val status = when {
        dispatching || starting -> stringResource(R.string.flight_starting)
        stopping -> stringResource(R.string.flight_stopping)
        !active && error != null -> error
        !active -> stringResource(R.string.flight_off)
        else -> when (connectionState) {
            ProxyConnectionState.LISTENING, ProxyConnectionState.DEGRADED -> stringResource(R.string.flight_listening)
            ProxyConnectionState.RELAYING_BYTES -> stringResource(R.string.flight_traffic)
            else -> stringResource(connectionState.label)
        }
    }
    val colors = MaterialTheme.colorScheme
    val statusColor = when {
        !active && error != null -> colors.error
        !ready -> colors.onSurface
        connectionState == ProxyConnectionState.NO_AVAILABLE_ROUTE -> colors.error
        connectionState == ProxyConnectionState.RELAYING_BYTES -> colors.primary
        else -> colors.onSurface
    }
    val statusDetail = if (ready && !stopping) when (connectionState) {
        ProxyConnectionState.NO_AVAILABLE_ROUTE -> stringResource(R.string.info_routes_unavailable_hint)
        else -> null
    } else null

    val port = savedPort.toIntOrNull() ?: 1443
    val bindIp = savedBindIp.trim().ifEmpty { "127.0.0.1" }
    val secretForUrl = savedSecretKey.trim().ifEmpty { "00000000000000000000000000000000" }
    val proxyUrl = "https://t.me/proxy?server=$bindIp&port=$port&secret=dd$secretForUrl"
    val fallbackError = stringResource(R.string.notification_start_failed)

    BoxWithConstraints(Modifier.fillMaxSize(), contentAlignment = Alignment.TopCenter) {
        val compactHeight = maxHeight < 380.dp
        Column(
            modifier = Modifier.widthIn(max = 540.dp).fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(start = 16.dp, end = 16.dp, top = 8.dp, bottom = 16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp)
        ) {
            Text(
                stringResource(R.string.nav_proxy),
                style = MaterialTheme.typography.headlineSmall,
                modifier = Modifier.heightIn(min = 48.dp).padding(top = 8.dp, end = 56.dp)
            )
            ConnectionHero(active, ready, stopping, status, statusColor, statusDetail, visible, compactHeight) {
                if (!dispatching && !starting && !stopping) {
                    view.performHapticFeedback(HapticFeedbackConstants.VIRTUAL_KEY)
                    dispatchError = null
                    if (isRunning) {
                        stopping = true
                        lifecycleOwner.lifecycleScope.launch {
                            try {
                                ProxyController.stop(context)
                                withTimeoutOrNull(7000) { ProxyService.isRunning.first { !it } }
                            } catch (cancelled: CancellationException) {
                                throw cancelled
                            } catch (_: Exception) {
                                Toast.makeText(context, R.string.flight_stop_failed, Toast.LENGTH_SHORT).show()
                            } finally {
                                stopping = false
                            }
                        }
                    } else {
                        dispatching = true
                        val previousError = ProxyService.startupError.value
                        lifecycleOwner.lifecycleScope.launch {
                            try {
                                val dispatched = ProxyController.startFromSavedSettings(context, showInvalidPortToast = true)
                                if (!dispatched) {
                                    dispatchError = context.getString(R.string.invalid_port)
                                } else if (withTimeoutOrNull(7000) {
                                        combine(ProxyService.startupInProgress, ProxyService.isRunning, ProxyService.startupError) {
                                            busy, running, failure -> busy || running || (failure != null && failure != previousError)
                                        }.first { it }
                                    } == null) {
                                    dispatchError = fallbackError
                                }
                            } catch (cancelled: CancellationException) {
                                throw cancelled
                            } catch (_: Exception) {
                                dispatchError = fallbackError
                            } finally {
                                dispatching = false
                            }
                        }
                    }
                }
            }

            AppSectionCard(
                contentPadding = PaddingValues(12.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp)
            ) {
                Button(
                    onClick = {
                        if (applyMode == "packages") applyToTelegramPackages(context, proxyUrl)
                        else openTelegram(context, proxyUrl)
                    },
                    enabled = ready,
                    modifier = Modifier.fillMaxWidth().heightIn(min = 48.dp),
                    shape = RoundedCornerShape(16.dp),
                    colors = ButtonDefaults.buttonColors(
                        containerColor = MaterialTheme.colorScheme.primaryContainer,
                        contentColor = MaterialTheme.colorScheme.onPrimaryContainer
                    )
                ) { Text(stringResource(R.string.apply_in_telegram)) }
                Row(
                    Modifier.fillMaxWidth().selectableGroup(),
                    horizontalArrangement = Arrangement.spacedBy(4.dp),
                    verticalAlignment = Alignment.CenterVertically
                ) {
                    val largeText = LocalDensity.current.fontScale >= 1.2f
                    ModeChip(
                        label = stringResource(R.string.telegram_apply_apps),
                        selected = applyMode == "packages",
                        modifier = Modifier.weight(if (largeText) 1.25f else 1f)
                    ) { applyMode = "packages" }
                    ModeChip(
                        label = stringResource(R.string.telegram_apply_link),
                        selected = applyMode == "link",
                        modifier = Modifier.weight(if (largeText) 0.75f else 1f)
                    ) { applyMode = "link" }
                    IconButton(
                        modifier = Modifier.size(48.dp),
                        onClick = {
                            val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as android.content.ClipboardManager
                            val clip = android.content.ClipData.newPlainText("Proxy", proxyUrl)
                            if (android.os.Build.VERSION.SDK_INT >= 24) {
                                clip.description.extras = android.os.PersistableBundle().apply {
                                    putBoolean("android.content.extra.IS_SENSITIVE", true)
                                }
                            }
                            clipboard.setPrimaryClip(clip)
                            Toast.makeText(context, R.string.copied, Toast.LENGTH_SHORT).show()
                        }
                    ) {
                        Icon(Icons.Default.ContentCopy, stringResource(R.string.copy_proxy_link),
                            tint = MaterialTheme.colorScheme.primary)
                    }
                }
                ProxyFacts(cfEnabled, pool, savedPort, sessionTraffic?.total)
            }
            if (ws80SuggestionActive && !savedCfPlainWs80 && !ws80Never && System.currentTimeMillis() >= ws80DismissedUntil) {
                AppSectionCard(
                    contentPadding = PaddingValues(14.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp)
                ) {
                    Text(
                        text = stringResource(R.string.ws80_suggest_title),
                        style = MaterialTheme.typography.titleSmall,
                        fontWeight = FontWeight.SemiBold,
                        color = MaterialTheme.colorScheme.onSurface
                    )
                    Text(
                        text = stringResource(R.string.ws80_suggest_message),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant
                    )
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                        verticalAlignment = Alignment.CenterVertically
                    ) {
                        Button(
                            onClick = {
                                ProxyService.clearWs80Suggestion(context)
                                onOpenWs80Settings()
                            },
                            shape = RoundedCornerShape(14.dp)
                        ) {
                            Text(stringResource(R.string.ws80_suggest_open_settings))
                        }
                        OutlinedButton(
                            onClick = {
                                ProxyService.clearWs80Suggestion(context)
                                lifecycleOwner.lifecycleScope.launch {
                                    settingsStore.dismissWs80SuggestionUntil(
                                        System.currentTimeMillis() + com.amurcanov.tgwsproxy.Ws80SuggestionPolicy.DISMISS_COOLDOWN_MS
                                    )
                                }
                            },
                            shape = RoundedCornerShape(14.dp)
                        ) {
                            Text(stringResource(R.string.ws80_suggest_later))
                        }
                    }
                    TextButton(
                        onClick = {
                            ProxyService.clearWs80Suggestion(context)
                            lifecycleOwner.lifecycleScope.launch {
                                settingsStore.setWs80SuggestionNever(true)
                            }
                        },
                        contentPadding = PaddingValues(horizontal = 4.dp, vertical = 0.dp)
                    ) {
                        Text(
                            text = stringResource(R.string.ws80_suggest_never),
                            style = MaterialTheme.typography.labelMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant
                        )
                    }
                }
            }
            RouteSettingsPanel(settingsStore, ready, connectionState == ProxyConnectionState.DEGRADED)
        }
    }
}

@Composable
private fun RouteSettingsPanel(settings: SettingsStore, running: Boolean, limitedRoutes: Boolean) {
    val sticky by settings.stickyRouting.collectAsStateWithLifecycle(initialValue = true)
    var expanded by rememberSaveable { mutableStateOf(false) }
    var snapshot by remember { mutableStateOf<JSONObject?>(null) }
    val context = LocalContext.current
    val scope = rememberCoroutineScope()

    suspend fun readSnapshot() {
        snapshot = withContext(Dispatchers.IO) {
            runCatching { NativeProxy.routeDiagnostics()?.let(::JSONObject) }.getOrNull()
        }
    }

    LaunchedEffect(expanded, running) {
        snapshot = null
        if (expanded && running) readSnapshot()
    }

    AppSectionCard(contentPadding = PaddingValues(horizontal = 16.dp, vertical = 8.dp)) {
        Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
            TextButton(onClick = { expanded = !expanded }, modifier = Modifier.fillMaxWidth().heightIn(min = 48.dp)) {
                Text(stringResource(R.string.route_panel_title))
                Spacer(Modifier.width(12.dp))
                Text(
                    stringResource(if (sticky) R.string.route_automatic else R.string.route_configured_order),
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    style = MaterialTheme.typography.labelMedium
                )
                Spacer(Modifier.weight(1f))
                Icon(if (expanded) Icons.Default.ExpandLess else Icons.Default.ExpandMore, contentDescription = null)
            }
            if (expanded) {
                if (running && limitedRoutes) {
                    Text(
                        stringResource(R.string.status_degraded),
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        style = MaterialTheme.typography.bodySmall
                    )
                }
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Column(Modifier.weight(1f)) {
                        Text(stringResource(R.string.route_automatic), style = MaterialTheme.typography.titleSmall)
                        Text(stringResource(R.string.route_policy_help), style = MaterialTheme.typography.bodySmall)
                    }
                    Switch(checked = sticky, onCheckedChange = { enabled ->
                        scope.launch {
                            settings.saveStickyRouting(enabled)
                            withContext(Dispatchers.IO) { NativeProxy.setStickyRouting(enabled) }
                        }
                    }, modifier = Modifier.padding(start = 12.dp))
                }
                snapshot?.let { details ->
                    val network = when (details.optInt("network")) {
                        1 -> stringResource(R.string.route_network_wifi)
                        2 -> stringResource(R.string.route_network_mobile)
                        3, 4 -> stringResource(R.string.route_network_other)
                        else -> stringResource(R.string.route_network_unknown)
                    }
                    Text(stringResource(R.string.route_sessions, network, details.optInt("active")),
                        style = MaterialTheme.typography.bodySmall)
                    FallbackLine(details.optJSONObject("cloudflare"), R.string.route_cf_wss)
                    FallbackLine(details.optJSONObject("worker"), R.string.route_worker_wss)
                    if (!details.isNull("lastPayloadSeconds")) {
                        Text(stringResource(R.string.route_last_payload, details.optLong("lastPayloadSeconds")),
                            style = MaterialTheme.typography.bodySmall)
                    }
                    details.optJSONObject("lastChange")?.let { change ->
                        val reason = when (change.optString("reason")) {
                            "previous_unhealthy" -> R.string.route_reason_previous_unhealthy
                            "sustained" -> R.string.route_reason_sustained
                            else -> R.string.route_reason_first_response
                        }
                        Text(stringResource(R.string.route_last_change, change.optInt("dc"),
                            routeKind(change.optString("kind")), stringResource(reason)),
                            style = MaterialTheme.typography.bodySmall)
                    }
                    val routes = details.optJSONArray("routes")
                    if (routes == null || routes.length() == 0) {
                        Text(stringResource(R.string.route_waiting_for_data), style = MaterialTheme.typography.bodySmall)
                    } else {
                        repeat(routes.length()) { index ->
                            val entry = routes.optJSONObject(index) ?: return@repeat
                            val dc = if (entry.optBoolean("media")) -entry.optInt("dc") else entry.optInt("dc")
                            val kind = routeKind(entry.optString("kind"))
                            val status = when (entry.optString("state")) {
                                "Healthy" -> stringResource(R.string.route_state_healthy)
                                "Degraded" -> stringResource(R.string.status_degraded)
                                "Cooldown", "Unavailable" -> stringResource(R.string.route_state_cooldown)
                                else -> stringResource(R.string.status_upstream_connecting)
                            }
                            Text(stringResource(R.string.route_preference_item, dc, kind, status),
                                style = MaterialTheme.typography.bodySmall)
                        }
                    }
                }
                if (snapshot == null) {
                    Text(stringResource(R.string.route_waiting_for_data), style = MaterialTheme.typography.bodySmall)
                }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    TextButton(enabled = running, onClick = { scope.launch { readSnapshot() } }) {
                        Text(stringResource(R.string.route_refresh))
                    }
                    TextButton(enabled = running, onClick = {
                        scope.launch {
                            val accepted = withContext(Dispatchers.IO) { NativeProxy.retryFailedRoutes() }
                            Toast.makeText(context, if (accepted) R.string.route_retry_accepted else R.string.route_retry_limited,
                                Toast.LENGTH_SHORT).show()
                            readSnapshot()
                        }
                    }) { Text(stringResource(R.string.route_retry)) }
                }
            }
        }
    }
}

@Composable
private fun FallbackLine(entry: JSONObject?, title: Int) {
    if (entry == null) return
    val state = entry.optString("state")
    val failure = entry.optString("lastFailure")
    if (state == "closed" && (failure.isEmpty() || failure == "none")) return
    val status = when (state) {
        "open" -> stringResource(R.string.route_cf_state_open)
        "half_open" -> stringResource(R.string.route_cf_state_half_open)
        else -> stringResource(R.string.route_cf_state_closed)
    }
    val reason = when (failure) {
        "timeout" -> stringResource(R.string.route_failure_timeout)
        "dns" -> stringResource(R.string.route_failure_dns)
        "tcp" -> stringResource(R.string.route_failure_tcp)
        "tls" -> stringResource(R.string.route_failure_tls)
        "websocket" -> stringResource(R.string.route_failure_websocket)
        "transport" -> stringResource(R.string.route_failure_transport)
        "other" -> stringResource(R.string.route_failure_other)
        else -> null
    }
    Text(
        if (reason == null) stringResource(R.string.route_fallback_status, stringResource(title), status)
        else stringResource(R.string.route_fallback_status_reason, stringResource(title), status, reason),
        style = MaterialTheme.typography.bodySmall
    )
}

@Composable
private fun routeKind(kind: String): String = when (kind) {
    "DirectWss" -> stringResource(R.string.route_direct_wss)
    "CfWss" -> stringResource(R.string.route_cf_wss)
    "WorkerWss" -> stringResource(R.string.route_worker_wss)
    else -> "TCP"
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun ProxyFacts(cfEnabled: Boolean, pool: Int, port: String, transferred: Long?) {
    val context = LocalContext.current
    FlowRow(
        modifier = Modifier.fillMaxWidth().padding(horizontal = 5.dp),
        horizontalArrangement = Arrangement.spacedBy(9.dp, Alignment.CenterHorizontally),
        verticalArrangement = Arrangement.spacedBy(3.dp)
    ) {
        val facts = listOf(
            if (cfEnabled) "CF" else stringResource(R.string.direct_mode),
            stringResource(R.string.pool_short, pool),
            stringResource(R.string.port_short, port),
            transferred?.let { stringResource(R.string.session_total_label, Formatter.formatShortFileSize(context, it)) }
                ?: "v${BuildConfig.VERSION_NAME}"
        )
        facts.forEach { fact ->
            Text(fact, style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }
}

@Composable
private fun ModeChip(
    label: String,
    selected: Boolean,
    modifier: Modifier = Modifier,
    onClick: () -> Unit
) {
    val colors = MaterialTheme.colorScheme
    Box(
        modifier = modifier.heightIn(min = 48.dp)
            .clip(RoundedCornerShape(14.dp))
            .background(if (selected) colors.surfaceVariant else colors.surface)
            .selectable(selected, role = Role.RadioButton, onClick = onClick)
            .padding(horizontal = 6.dp, vertical = 6.dp),
        contentAlignment = Alignment.Center
    ) {
        Text(
            label,
            style = MaterialTheme.typography.labelLarge,
            color = if (selected) colors.primary else colors.onSurfaceVariant,
            fontWeight = if (selected) FontWeight.SemiBold else FontWeight.Normal
        )
    }
}

private val telegramPackages = listOf(
    "org.telegram.messenger",
    "com.radolyn.ayugram",
    "com.exteragram.messenger",
    "org.telegram.plus",
    "ir.ilmili.telegraph",
    "org.telegram.BifToGram",
    "tw.nekomimi.nekogram",
    "xyz.nextalone.nagram",
    "uz.unnarsx.cherrygram",
    "org.telegram.mdgram",
    "org.forkclient.messenger.beta",
    "app.nicegram",
    "top.qwq2333.nullgram",
    "com.iMe.android",
    "ru.dahl.messenger",
    "com.scriptsaz.litegram",
    "org.thunderdog.challegram"
)

private fun applyToTelegramPackages(context: Context, url: String) {
    val pm = context.packageManager
    val availablePackages = telegramPackages.filter {
        try {
            pm.getPackageInfo(it, 0)
            true
        } catch (e: PackageManager.NameNotFoundException) {
            false
        }
    }

    if (availablePackages.isEmpty()) {
        Toast.makeText(context, "Клиенты не найдены", Toast.LENGTH_SHORT).show()
        return
    }

    val targetedIntents = availablePackages.map { pkg ->
        Intent(Intent.ACTION_VIEW, Uri.parse(url)).apply {
            setPackage(pkg)
        }
    }

    if (targetedIntents.size == 1) {
        val intent = targetedIntents.first().apply {
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        }
        try {
            context.startActivity(intent)
        } catch (e: Exception) {
            Toast.makeText(context, "Ошибка при открытии клиента", Toast.LENGTH_SHORT).show()
        }
    } else {
        val chooserIntent = Intent.createChooser(targetedIntents.first(), "Выберите клиент")
        chooserIntent.putExtra(Intent.EXTRA_INITIAL_INTENTS, targetedIntents.drop(1).toTypedArray())
        chooserIntent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        try {
            context.startActivity(chooserIntent)
        } catch (e: Exception) {
            Toast.makeText(context, "Ошибка при выборе клиента", Toast.LENGTH_SHORT).show()
        }
    }
}
