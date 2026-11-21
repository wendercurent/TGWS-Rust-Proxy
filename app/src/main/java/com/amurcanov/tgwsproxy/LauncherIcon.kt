package com.amurcanov.tgwsproxy

import android.content.ComponentName
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

internal enum class LauncherIcon(
    private val alias: String,
    val label: Int,
    val foreground: Int,
    val background: Int
) {
    Fold("LauncherFold", R.string.launcher_fold, R.drawable.ic_launcher_fg, R.color.launcher_fold_background),
    Relay("LauncherRelay", R.string.launcher_relay, R.drawable.ic_launcher_relay_fg, R.color.launcher_relay_background),
    Network("LauncherNetwork", R.string.launcher_grid, R.drawable.ic_launcher_grid_fg, R.color.launcher_grid_background);

    private fun component(context: Context) = ComponentName(context.packageName, "com.amurcanov.tgwsproxy.$alias")

    companion object {
        private var selection: MutableStateFlow<LauncherIcon>? = null

        @Synchronized
        fun observe(context: Context): StateFlow<LauncherIcon> = selectionState(context).asStateFlow()

        private fun selectionState(context: Context): MutableStateFlow<LauncherIcon> =
            selection ?: MutableStateFlow(current(context)).also { selection = it }

        fun current(context: Context): LauncherIcon = entries.firstOrNull { icon ->
            when (context.packageManager.getComponentEnabledSetting(icon.component(context))) {
                PackageManager.COMPONENT_ENABLED_STATE_ENABLED -> true
                PackageManager.COMPONENT_ENABLED_STATE_DEFAULT -> icon == Relay
                else -> false
            }
        } ?: Relay

        @Synchronized
        fun select(context: Context, choice: LauncherIcon) {
            val manager = context.packageManager
            val flags = PackageManager.DONT_KILL_APP
            val previous = current(context)
            val ordered = listOf(choice) + entries.filter { it != choice }
            if (Build.VERSION.SDK_INT >= 33) {
                manager.setComponentEnabledSettings(ordered.map { icon ->
                    PackageManager.ComponentEnabledSetting(icon.component(context), icon.stateFor(choice), flags)
                })
            } else {
                try {
                    for (icon in ordered) {
                        manager.setComponentEnabledSetting(icon.component(context), icon.stateFor(choice), flags)
                    }
                } catch (failure: RuntimeException) {
                    // Re-enable the old alias before disabling the new one.
                    runCatching {
                        manager.setComponentEnabledSetting(previous.component(context), previous.stateFor(previous), flags)
                        for (icon in entries.filter { it != previous }) {
                            manager.setComponentEnabledSetting(icon.component(context), icon.stateFor(previous), flags)
                        }
                    }
                    selectionState(context).value = current(context)
                    throw failure
                }
            }
            selectionState(context).value = choice
        }

        private fun LauncherIcon.stateFor(choice: LauncherIcon) =
            if (this == choice) PackageManager.COMPONENT_ENABLED_STATE_ENABLED
            else PackageManager.COMPONENT_ENABLED_STATE_DISABLED
    }
}
