package com.amurcanov.tgwsproxy.ui

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Build
import android.widget.Toast
import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.selection.selectableGroup
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.colorResource
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.amurcanov.tgwsproxy.AppReleaseInfo
import com.amurcanov.tgwsproxy.BuildConfig
import com.amurcanov.tgwsproxy.FORK_UPDATES_ENABLED
import com.amurcanov.tgwsproxy.LauncherIcon
import com.amurcanov.tgwsproxy.LogEntry
import com.amurcanov.tgwsproxy.LogManager
import com.amurcanov.tgwsproxy.R
import com.amurcanov.tgwsproxy.SettingsStore
import com.amurcanov.tgwsproxy.UPDATE_DIALOG_ACTION_POSTPONED
import com.amurcanov.tgwsproxy.UPDATE_DIALOG_ACTION_UPDATE
import com.amurcanov.tgwsproxy.fetchLatestReleaseInfo
import com.amurcanov.tgwsproxy.isNewerVersion
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

private const val AndroidForkRepoUrl = "https://github.com/amurcanov/tg-ws-proxy-android"
private const val DeveloperProfileUrl = "https://github.com/amurcanov"
private const val OriginalProjectUrl = "https://github.com/Flowseal/tg-ws-proxy"
private const val ProxyReferenceUrl = "https://github.com/Flowseal/tg-ws-proxy/issues/389"

@Composable
fun InfoTab(settingsStore: SettingsStore) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var showHelp by rememberSaveable { mutableStateOf(false) }
    var showLicenses by rememberSaveable { mutableStateOf(false) }
    val iconSelection = remember(context) { LauncherIcon.observe(context) }
    val activeIcon by iconSelection.collectAsStateWithLifecycle()
    var changingIcon by remember { mutableStateOf(false) }
    val savedPort by settingsStore.port.collectAsStateWithLifecycle("1443")
    val savedPoolSize by settingsStore.poolSize.collectAsStateWithLifecycle(4)
    val savedCfEnabled by settingsStore.cfproxyEnabled.collectAsStateWithLifecycle(true)
    val updateLatestVersion by settingsStore.updateLatestVersion.collectAsStateWithLifecycle("")
    val updateLastError by settingsStore.updateLastError.collectAsStateWithLifecycle("")
    var checking by remember { mutableStateOf(false) }
    var pendingRelease by remember { mutableStateOf<AppReleaseInfo?>(null) }
    val releaseCheckAvailable = FORK_UPDATES_ENABLED && BuildConfig.RELEASE_REPO.isNotBlank()
    val sourceUrl = if (BuildConfig.RELEASE_REPO.isNotBlank()) "https://github.com/${BuildConfig.RELEASE_REPO}" else AndroidForkRepoUrl
    val currentVersion = "v${BuildConfig.VERSION_NAME.removePrefix("v")}"
    val colors = MaterialTheme.colorScheme

    Box(Modifier.fillMaxSize(), contentAlignment = Alignment.TopCenter) {
        Column(
            Modifier.widthIn(max = 560.dp).fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 16.dp, vertical = 12.dp),
            verticalArrangement = Arrangement.spacedBy(14.dp)
        ) {
            Text(
                stringResource(R.string.info),
                Modifier.heightIn(min = 48.dp).padding(top = 5.dp),
                style = MaterialTheme.typography.headlineMedium
            )
            FrostedHeader(Modifier.fillMaxWidth()) {
                Column(Modifier.padding(17.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                        LauncherMark(LauncherIcon.Relay, Modifier.size(60.dp))
                        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(3.dp)) {
                            Text(stringResource(R.string.app_name), style = MaterialTheme.typography.titleLarge)
                            Text(
                                stringResource(R.string.info_version_build, BuildConfig.VERSION_NAME, BuildConfig.VERSION_CODE),
                                style = MaterialTheme.typography.labelMedium,
                                color = colors.primary
                            )
                        }
                    }
                    Text(stringResource(R.string.hero_description), style = MaterialTheme.typography.bodyMedium, color = colors.onSurfaceVariant)
                    if (BuildConfig.OWNER_GITHUB_LOGIN.isNotBlank()) {
                        Text(stringResource(R.string.maintained_by, BuildConfig.OWNER_GITHUB_LOGIN), style = MaterialTheme.typography.labelMedium)
                    }
                }
            }

            AppSectionCard(contentPadding = PaddingValues(12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Icon(painterResource(R.drawable.ic_tabler_palette), null, tint = colors.primary, modifier = Modifier.size(20.dp))
                    Text(stringResource(R.string.info_launcher_icons), style = MaterialTheme.typography.titleMedium)
                }
                Row(Modifier.fillMaxWidth().selectableGroup(), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    LauncherIcon.entries.forEach { icon ->
                        val selected = activeIcon == icon
                        val accessibilityLabel = stringResource(icon.label)
                        Surface(
                            modifier = Modifier.weight(1f),
                            shape = RoundedCornerShape(13.dp),
                            color = if (selected) colors.primaryContainer else colors.surface,
                            border = BorderStroke(1.dp, if (selected) colors.primary else colors.outlineVariant)
                        ) {
                            Column(
                                Modifier.fillMaxWidth().heightIn(min = 66.dp)
                                    .selectable(selected, enabled = !changingIcon, role = Role.RadioButton) {
                                        if (!selected && !changingIcon) {
                                            changingIcon = true
                                            scope.launch {
                                                val change = withContext(Dispatchers.IO) {
                                                    runCatching { LauncherIcon.select(context, icon) }
                                                }
                                                changingIcon = false
                                                if (change.isFailure) Toast.makeText(context, R.string.launcher_icon_failed, Toast.LENGTH_SHORT).show()
                                            }
                                        }
                                    }
                                    .semantics { contentDescription = accessibilityLabel }
                                    .padding(horizontal = 3.dp, vertical = 8.dp),
                                horizontalAlignment = Alignment.CenterHorizontally,
                                verticalArrangement = Arrangement.Center
                            ) {
                                LauncherMark(icon, Modifier.size(48.dp))
                            }
                        }
                    }
                }
                Text(
                    stringResource(R.string.info_launcher_icons_help),
                    style = MaterialTheme.typography.bodySmall, color = colors.onSurfaceVariant
                )
            }

            InfoGroup(R.string.info_support_heading) {
                InfoRow(R.drawable.ic_tabler_lifebuoy, R.string.help, stringResource(R.string.help_subtitle)) { showHelp = true }
                InfoDivider()
                InfoRow(R.drawable.ic_tabler_file_description, R.string.build_report, stringResource(R.string.report_subtitle)) {
                    val logs = if (BuildConfig.DEBUG) LogManager.logs.value else emptyList()
                    val report = buildSupportReport(savedPort, savedPoolSize, savedCfEnabled, logs, context)
                    context.getSystemService(ClipboardManager::class.java)
                        ?.setPrimaryClip(ClipData.newPlainText("TGWS support report", report))
                    Toast.makeText(context, R.string.copied, Toast.LENGTH_SHORT).show()
                }
                if (BuildConfig.SUPPORT_URL.startsWith("https://")) {
                    InfoDivider()
                    InfoRow(R.drawable.ic_tabler_external_link, R.string.raise_issue, stringResource(R.string.open_support)) {
                        openUrlInBrowser(context, BuildConfig.SUPPORT_URL)
                    }
                }
            }

            InfoGroup(R.string.about_project) {
                InfoRow(
                    R.drawable.ic_tabler_brand_github,
                    if (BuildConfig.RELEASE_REPO.isNotBlank()) R.string.info_project_source else R.string.android_fork_repo,
                    stringResource(R.string.info_project_source_summary)
                ) { openUrlInBrowser(context, sourceUrl) }
                InfoDivider()
                InfoRow(R.drawable.ic_tabler_scale, R.string.info_licenses, stringResource(R.string.info_licenses_summary)) {
                    showLicenses = true
                }
                if (releaseCheckAvailable) {
                    InfoDivider()
                    val subtitle = when {
                        checking -> stringResource(R.string.update_checking)
                        updateLatestVersion.isNotBlank() && isNewerVersion(currentVersion, updateLatestVersion) ->
                            stringResource(R.string.update_available_on_github, updateLatestVersion)
                        updateLatestVersion.isNotBlank() -> stringResource(R.string.update_latest_version, updateLatestVersion)
                        updateLastError.isNotBlank() -> stringResource(R.string.update_last_check_failed)
                        else -> stringResource(R.string.update_check_manually)
                    }
                    InfoRow(R.drawable.ic_tabler_refresh, R.string.check_updates, subtitle, enabled = !checking) {
                        checking = true
                        scope.launch {
                            try {
                                val checkedAt = System.currentTimeMillis()
                                val release = fetchLatestReleaseInfo(currentVersion)
                                settingsStore.saveUpdateState(
                                    checkedAt,
                                    release?.versionTag.orEmpty(),
                                    if (release == null) context.getString(R.string.update_check_failed_short) else ""
                                )
                                when {
                                    release == null -> {
                                        val message = if (updateLatestVersion.isNotBlank()) {
                                            context.getString(R.string.update_check_failed_known, updateLatestVersion)
                                        } else context.getString(R.string.update_check_failed)
                                        Toast.makeText(context, message, Toast.LENGTH_SHORT).show()
                                    }
                                    isNewerVersion(currentVersion, release.versionTag) -> {
                                        settingsStore.saveUpdateDialogShown(release.versionTag, checkedAt)
                                        pendingRelease = release
                                    }
                                    else -> Toast.makeText(context, context.getString(R.string.update_already_latest, release.versionTag), Toast.LENGTH_SHORT).show()
                                }
                            } finally {
                                checking = false
                            }
                        }
                    }
                }
            }
        }
    }

    pendingRelease?.let { release ->
        AppUpdateDialog(
            release = release,
            onPostpone = {
                pendingRelease = null
                Toast.makeText(context, R.string.update_postponed_24h, Toast.LENGTH_SHORT).show()
                scope.launch {
                    val now = System.currentTimeMillis()
                    settingsStore.saveUpdatePostpone(release.versionTag, now + 24L * 60L * 60L * 1000L)
                    settingsStore.saveUpdateDialogAction(release.versionTag, UPDATE_DIALOG_ACTION_POSTPONED, now)
                }
            },
            onUpdate = {
                pendingRelease = null
                scope.launch {
                    settingsStore.saveUpdateDialogAction(release.versionTag, UPDATE_DIALOG_ACTION_UPDATE, System.currentTimeMillis())
                    openUrlInBrowser(context, release.releaseUrl)
                }
            }
        )
    }

    if (showHelp) {
        InfoDialog(R.string.help, onDismiss = { showHelp = false }) {
            val topics = listOf(
                R.string.help_slow_connect_title to R.string.help_slow_connect_text,
                R.string.help_auto_dc_title to R.string.help_auto_dc_text,
                R.string.settings_cloudflare to R.string.help_cloudflare_text,
                R.string.ws_pool to R.string.help_ws_pool_text,
                R.string.secret_key to R.string.help_secret_key_text,
                R.string.autostart to R.string.help_autostart_text
            ) + if (BuildConfig.DEBUG) listOf(R.string.experimental_mode to R.string.help_experimental_text) else emptyList()
            topics.forEach { (title, description) ->
                Column(verticalArrangement = Arrangement.spacedBy(3.dp)) {
                    Text(stringResource(title), style = MaterialTheme.typography.titleSmall, color = colors.primary)
                    Text(stringResource(description), style = MaterialTheme.typography.bodyMedium)
                }
            }
            TextButton(onClick = { openUrlInBrowser(context, ProxyReferenceUrl) }) { Text(stringResource(R.string.useful_material)) }
        }
    }
    if (showLicenses) {
        val tablerLicense = remember(context) {
            context.resources.openRawResource(R.raw.tabler_icons_license).bufferedReader().use { it.readText() }.trim()
        }
        InfoDialog(R.string.info_licenses, onDismiss = { showLicenses = false }) {
            Text(stringResource(R.string.fork_lineage), style = MaterialTheme.typography.bodyMedium)
            TextButton(onClick = { openUrlInBrowser(context, "$sourceUrl/blob/main/LICENSE") }) { Text("GPL-3.0") }
            TextButton(onClick = { openUrlInBrowser(context, DeveloperProfileUrl) }) { Text(stringResource(R.string.android_author)) }
            TextButton(onClick = { openUrlInBrowser(context, OriginalProjectUrl) }) { Text(stringResource(R.string.original_tg_ws_proxy)) }
            Text(stringResource(R.string.info_tabler_credit), style = MaterialTheme.typography.titleSmall)
            Text(tablerLicense, style = MaterialTheme.typography.bodySmall)
        }
    }
}

@Composable
private fun LauncherMark(icon: LauncherIcon, modifier: Modifier = Modifier) {
    Box(
        modifier.clip(RoundedCornerShape(14.dp)).background(colorResource(icon.background)),
        contentAlignment = Alignment.Center
    ) {
        Image(painterResource(icon.foreground), null, Modifier.fillMaxSize())
    }
}

@Composable
private fun InfoGroup(@StringRes title: Int, content: @Composable ColumnScope.() -> Unit) {
    AppSectionCard(contentPadding = PaddingValues(horizontal = 12.dp, vertical = 8.dp), verticalArrangement = Arrangement.spacedBy(0.dp)) {
        Text(
            stringResource(title), Modifier.padding(start = 6.dp, top = 4.dp, bottom = 6.dp),
            style = MaterialTheme.typography.titleMedium
        )
        content()
    }
}

@Composable
private fun InfoDivider() {
    HorizontalDivider(Modifier.padding(start = 45.dp, end = 6.dp), color = MaterialTheme.colorScheme.outlineVariant)
}

@Composable
private fun InfoRow(
    @DrawableRes icon: Int, @StringRes title: Int, subtitle: String, enabled: Boolean = true, onClick: () -> Unit
) {
    val colors = MaterialTheme.colorScheme
    Row(
        Modifier.fillMaxWidth().heightIn(min = 64.dp)
            .clip(RoundedCornerShape(12.dp)).clickable(enabled = enabled, onClick = onClick)
            .padding(horizontal = 6.dp, vertical = 7.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp)
    ) {
        Icon(painterResource(icon), null, tint = colors.primary, modifier = Modifier.size(22.dp))
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(stringResource(title), style = MaterialTheme.typography.titleSmall)
            Text(subtitle, style = MaterialTheme.typography.bodySmall, color = colors.onSurfaceVariant)
        }
        Icon(painterResource(R.drawable.ic_tabler_chevron_right), null, tint = colors.onSurfaceVariant, modifier = Modifier.size(16.dp))
    }
}

@Composable
private fun InfoDialog(@StringRes title: Int, onDismiss: () -> Unit, content: @Composable ColumnScope.() -> Unit) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(title)) },
        text = {
            Column(Modifier.heightIn(max = 440.dp).verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(12.dp), content = content)
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.understood)) } }
    )
}

private fun buildSupportReport(port: String, poolSize: Int, cfEnabled: Boolean, logs: List<LogEntry>, context: Context): String = buildString {
    appendLine(context.getString(R.string.report_app_version, BuildConfig.VERSION_NAME))
    appendLine(context.getString(R.string.report_android, Build.VERSION.RELEASE, Build.VERSION.SDK_INT))
    appendLine("ABI: ${Build.SUPPORTED_ABIS.firstOrNull().orEmpty()}")
    val mode = if (cfEnabled) "Cloudflare" else context.getString(R.string.report_mode_direct)
    appendLine(context.getString(R.string.report_mode, mode))
    appendLine(context.getString(R.string.report_ws_pool, poolSize))
    appendLine(context.getString(R.string.report_port, port.trim().ifBlank { "1443" }))
    if (BuildConfig.DEBUG) {
        append(context.getString(R.string.report_error_count, logs.count { it.priority >= 5 }))
    }
}
