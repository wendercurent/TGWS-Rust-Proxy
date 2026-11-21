package com.amurcanov.tgwsproxy.ui

import android.os.Build
import android.app.Activity
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ColorScheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.SideEffect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.lerp
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalView
import androidx.core.view.WindowCompat

import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp
import com.amurcanov.tgwsproxy.R
import com.amurcanov.tgwsproxy.DEFAULT_THEME_PALETTE

// ═══ System Font Family ═══
val AppFontFamily = FontFamily.Default

// ═══ Типография ═══
val TgWsProxyTypography = Typography(
    displayLarge = TextStyle(fontFamily = AppFontFamily, fontWeight = FontWeight.Bold, fontSize = 57.sp, lineHeight = 64.sp),
    displayMedium = TextStyle(fontFamily = AppFontFamily, fontWeight = FontWeight.Bold, fontSize = 45.sp, lineHeight = 52.sp),
    displaySmall = TextStyle(fontFamily = AppFontFamily, fontWeight = FontWeight.Bold, fontSize = 36.sp, lineHeight = 44.sp),
    headlineLarge = TextStyle(fontFamily = AppFontFamily, fontWeight = FontWeight.SemiBold, fontSize = 32.sp, lineHeight = 40.sp),
    headlineMedium = TextStyle(fontFamily = AppFontFamily, fontWeight = FontWeight.SemiBold, fontSize = 28.sp, lineHeight = 36.sp),
    headlineSmall = TextStyle(fontFamily = AppFontFamily, fontWeight = FontWeight.SemiBold, fontSize = 24.sp, lineHeight = 32.sp),
    titleLarge = TextStyle(fontFamily = AppFontFamily, fontWeight = FontWeight.SemiBold, fontSize = 22.sp, lineHeight = 28.sp),
    titleMedium = TextStyle(fontFamily = AppFontFamily, fontWeight = FontWeight.SemiBold, fontSize = 16.sp, lineHeight = 24.sp, letterSpacing = 0.15.sp),
    titleSmall = TextStyle(fontFamily = AppFontFamily, fontWeight = FontWeight.Medium, fontSize = 14.sp, lineHeight = 20.sp, letterSpacing = 0.1.sp),
    bodyLarge = TextStyle(fontFamily = AppFontFamily, fontWeight = FontWeight.Normal, fontSize = 16.sp, lineHeight = 24.sp, letterSpacing = 0.5.sp),
    bodyMedium = TextStyle(fontFamily = AppFontFamily, fontWeight = FontWeight.Normal, fontSize = 14.sp, lineHeight = 20.sp, letterSpacing = 0.25.sp),
    bodySmall = TextStyle(fontFamily = AppFontFamily, fontWeight = FontWeight.Normal, fontSize = 12.sp, lineHeight = 16.sp, letterSpacing = 0.4.sp),
    labelLarge = TextStyle(fontFamily = AppFontFamily, fontWeight = FontWeight.Medium, fontSize = 14.sp, lineHeight = 20.sp, letterSpacing = 0.1.sp),
    labelMedium = TextStyle(fontFamily = AppFontFamily, fontWeight = FontWeight.Medium, fontSize = 12.sp, lineHeight = 16.sp, letterSpacing = 0.5.sp),
    labelSmall = TextStyle(fontFamily = AppFontFamily, fontWeight = FontWeight.Medium, fontSize = 11.sp, lineHeight = 16.sp, letterSpacing = 0.5.sp),
)

// Palette keys remain compatible with saved preferences.
private val MidnightLight = lightColorScheme(
    primary = Color(0xFF844829),
    onPrimary = Color(0xFFFFFFFF),
    primaryContainer = Color(0xFFF8C7AA),
    onPrimaryContainer = Color(0xFF351C10),
    secondary = Color(0xFF386277),
    onSecondary = Color.White,
    secondaryContainer = Color(0xFFCFE7F1),
    onSecondaryContainer = Color(0xFF182E3C),
    tertiary = Color(0xFF596182),
    background = Color(0xFFF5EEE9),
    onBackground = Color(0xFF22232C),
    surface = Color(0xFFFCF7F2),
    onSurface = Color(0xFF22232C),
    surfaceVariant = Color(0xFFECE3DE),
    onSurfaceVariant = Color(0xFF57525A),
    outline = Color(0xFF74696D),
    outlineVariant = Color(0xFFD8C8C2)
)

private val MidnightDark = darkColorScheme(
    primary = FlightCopper,
    onPrimary = Color(0xFF2C1B15),
    primaryContainer = Color(0xFF50362B),
    onPrimaryContainer = Color(0xFFFFDDC9),
    secondary = FlightSky,
    onSecondary = Color(0xFF0D2C38),
    secondaryContainer = Color(0xFF253E4B),
    onSecondaryContainer = Color(0xFFD1EAF5),
    tertiary = Color(0xFFC4B9E7),
    background = Color(0xFF15161B),
    onBackground = Color(0xFFF6F3F0),
    surface = Color(0xFF22242C),
    onSurface = Color(0xFFF6F3F0),
    surfaceVariant = Color(0xFF30323B),
    onSurfaceVariant = Color(0xFFB8B8C0),
    outline = Color(0xFF8E8791),
    outlineVariant = Color(0xFF494550)
)

internal enum class AppPalette(val key: String, val label: Int, val accent: Color) {
    North("indigo", R.string.palette_north, Color(0xFF9CDDF4)),
    Copper("espresso", R.string.palette_copper, Color(0xFFF4A878)),
    Moss("forest", R.string.palette_moss, Color(0xFFB1DBAB))
}

private fun getAppColorScheme(palette: String, isDark: Boolean): ColorScheme {
    val base = if (isDark) MidnightDark else MidnightLight
    val scheme = when (palette) {
        AppPalette.Copper.key -> base
        AppPalette.Moss.key -> if (isDark) base.copy(
            primary = AppPalette.Moss.accent, onPrimary = Color(0xFF203420),
            primaryContainer = Color(0xFF335441), onPrimaryContainer = Color(0xFFDDF0D7),
            secondary = Color(0xFFA3D3B9), onSecondary = Color(0xFF173127),
            secondaryContainer = Color(0xFF294B3C), onSecondaryContainer = Color(0xFFD5F1E2),
            tertiary = Color(0xFFD1D9A0), onTertiary = Color(0xFF30371F),
            background = Color(0xFF17211E), surface = Color(0xFF22312B),
            onBackground = Color(0xFFEDF5E9), onSurface = Color(0xFFEDF5E9),
            surfaceVariant = Color(0xFF30443A), onSurfaceVariant = Color(0xFFBCCCBF),
            outline = Color(0xFF88A094), outlineVariant = Color(0xFF405C4E)
        ) else base.copy(
            primary = Color(0xFF37643C), onPrimary = Color.White,
            primaryContainer = Color(0xFFD2EACD), onPrimaryContainer = Color(0xFF1E3B24),
            secondary = Color(0xFF38664E), onSecondary = Color.White,
            secondaryContainer = Color(0xFFDAEDDF), onSecondaryContainer = Color(0xFF233E30),
            tertiary = Color(0xFF53612D), onTertiary = Color.White,
            background = Color(0xFFEEF3E9), surface = Color(0xFFFAFCF7),
            onBackground = Color(0xFF233329), onSurface = Color(0xFF233329),
            surfaceVariant = Color(0xFFE0EBDE), onSurfaceVariant = Color(0xFF4A624F),
            outline = Color(0xFF728775), outlineVariant = Color(0xFFC3D4C1)
        )
        else -> if (isDark) base.copy(
            primary = AppPalette.North.accent, onPrimary = Color(0xFF132C38),
            primaryContainer = Color(0xFF294B60), onPrimaryContainer = Color(0xFFD3EFFC),
            secondary = Color(0xFFACC7E8), onSecondary = Color(0xFF1E354D),
            secondaryContainer = Color(0xFF2D4259), onSecondaryContainer = Color(0xFFDDEBFC),
            tertiary = Color(0xFFC5BDEB), onTertiary = Color(0xFF312B48),
            background = Color(0xFF141B24), surface = Color(0xFF1E2B37),
            onBackground = Color(0xFFEAF4FA), onSurface = Color(0xFFEAF4FA),
            surfaceVariant = Color(0xFF2B4051), onSurfaceVariant = Color(0xFFB7C9D6),
            outline = Color(0xFF8399AA), outlineVariant = Color(0xFF354F60)
        ) else base.copy(
            primary = Color(0xFF215F79), onPrimary = Color.White,
            primaryContainer = Color(0xFFCFEAF7), onPrimaryContainer = Color(0xFF173C50),
            secondary = Color(0xFF425F81), onSecondary = Color.White,
            secondaryContainer = Color(0xFFDCEAFB), onSecondaryContainer = Color(0xFF293F58),
            tertiary = Color(0xFF5F577B), onTertiary = Color.White,
            background = Color(0xFFEDF4F7), surface = Color(0xFFF8FBFC),
            onBackground = Color(0xFF192B36), onSurface = Color(0xFF192B36),
            surfaceVariant = Color(0xFFDCEBF3), onSurfaceVariant = Color(0xFF46606E),
            outline = Color(0xFF6C8490), outlineVariant = Color(0xFFB8D0DB)
        )
    }
    return scheme.copy(
        surfaceTint = scheme.primary,
        surfaceBright = lerp(scheme.surface, scheme.surfaceVariant, 0.36f),
        surfaceDim = scheme.background,
        surfaceContainerLowest = scheme.background,
        surfaceContainerLow = lerp(scheme.background, scheme.surface, 0.65f),
        surfaceContainer = scheme.surface,
        surfaceContainerHigh = lerp(scheme.surface, scheme.surfaceVariant, 0.50f),
        surfaceContainerHighest = scheme.surfaceVariant
    )
}

// ═══ Расширенные цвета для кастомных элементов ═══
object AppColors {
    val connected = Color(0xFF4CAF50)
    val connectedContainer = Color(0xFF4CAF50).copy(alpha = 0.12f)
    val onConnected = Color(0xFF1B5E20)

    val connectedDark = Color(0xFF81C784)
    val connectedContainerDark = Color(0xFF81C784).copy(alpha = 0.15f)
    val onConnectedDark = Color(0xFFC8E6C9)

    val warning = Color(0xFFFFA726)
    val warningDark = Color(0xFFFFCC80)

    val terminalBg = Color(0xFF1A1A2E)
    val terminalBgDark = Color(0xFF0D0D1A)
    val terminalText = Color(0xFFE0E0E0)
    val terminalGreen = Color(0xFF4CAF50)
    val terminalBlue = Color(0xFF42A5F5)
    val terminalRed = Color(0xFFEF5350)
    val terminalOrange = Color(0xFFFF7043)
    val terminalYellow = Color(0xFFFFC107)
    val terminalCounter = Color(0xFF1E88E5)

    val github = Color(0xFF24292E)
    val githubDark = Color(0xFF333C47)

    val donate = Color(0xFF8B3FFD)
}

@Composable
fun TgWsProxyTheme(
    themeMode: String = "system",
    dynamicColor: Boolean = false,
    themePalette: String = DEFAULT_THEME_PALETTE,
    content: @Composable () -> Unit
) {
    val darkTheme = when (themeMode) {
        "dark" -> true
        "light" -> false
        else -> isSystemInDarkTheme()
    }

    val colorScheme = when {
        dynamicColor && Build.VERSION.SDK_INT >= Build.VERSION_CODES.S -> {
            val context = LocalContext.current
            if (darkTheme) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)
        }
        else -> getAppColorScheme(themePalette, darkTheme)
    }
    val view = LocalView.current

    if (!view.isInEditMode) {
        SideEffect {
            val window = (view.context as Activity).window
            val navigationBarColor = if (darkTheme) {
                Color.Transparent
            } else {
                lerp(colorScheme.background, colorScheme.surface, 0.55f)
            }
            @Suppress("DEPRECATION")
            window.statusBarColor = Color.Transparent.toArgb()
            @Suppress("DEPRECATION")
            window.navigationBarColor = navigationBarColor.toArgb()
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                @Suppress("DEPRECATION")
                window.isNavigationBarContrastEnforced = false
                @Suppress("DEPRECATION")
                window.isStatusBarContrastEnforced = false
            }
            WindowCompat.getInsetsController(window, view).apply {
                isAppearanceLightStatusBars = !darkTheme
                isAppearanceLightNavigationBars = !darkTheme
            }
        }
    }

    MaterialTheme(
        colorScheme = colorScheme,
        typography = TgWsProxyTypography,
        content = content
    )
}
