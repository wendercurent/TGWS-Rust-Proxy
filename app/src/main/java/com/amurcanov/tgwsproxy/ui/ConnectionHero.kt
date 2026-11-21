package com.amurcanov.tgwsproxy.ui

import android.animation.ValueAnimator
import android.os.Build
import android.provider.Settings
import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.snap
import androidx.compose.animation.core.tween
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithCache
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ColorFilter
import androidx.compose.ui.graphics.ColorMatrix
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.drawscope.clipPath
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.amurcanov.tgwsproxy.R

internal val FlightCopper = Color(0xFFF4A878)
internal val FlightSky = Color(0xFF7BB9D5)

@Composable
internal fun ConnectionHero(
    active: Boolean,
    ready: Boolean,
    stopping: Boolean,
    status: String,
    statusColor: Color,
    statusDetail: String?,
    visible: Boolean,
    compactHeight: Boolean,
    onPower: () -> Unit
) {
    val colors = MaterialTheme.colorScheme
    Surface(
        modifier = Modifier.fillMaxWidth(),
        shape = RoundedCornerShape(26.dp),
        color = colors.surface,
        contentColor = colors.onSurface,
        border = BorderStroke(1.dp, colors.outlineVariant)
    ) {
        Box {
            PolarArtwork(Modifier.matchParentSize())
            Column(
                modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 14.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.spacedBy(6.dp)
            ) {
                ProxyLogoButton(active, ready, !stopping && (!active || ready), visible, compactHeight, onPower)
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(8.dp)
                ) {
                    Box(Modifier.size(7.dp).background(statusColor, CircleShape))
                    Text(
                        status,
                        style = MaterialTheme.typography.titleMedium,
                        color = statusColor,
                        textAlign = TextAlign.Center,
                        modifier = Modifier.semantics { liveRegion = LiveRegionMode.Polite }
                    )
                }
                if (statusDetail != null && !compactHeight) {
                    Text(
                        statusDetail,
                        style = MaterialTheme.typography.bodySmall,
                        color = colors.onSurfaceVariant,
                        textAlign = TextAlign.Center
                    )
                }
            }
        }
    }
}

@Composable
private fun ProxyLogoButton(
    active: Boolean,
    ready: Boolean,
    enabled: Boolean,
    visible: Boolean,
    compactHeight: Boolean,
    onPower: () -> Unit
) {
    val context = LocalContext.current
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    val stage by lifecycle.currentStateFlow.collectAsState()
    val systemAllowsMotion = if (Build.VERSION.SDK_INT >= 26) ValueAnimator.areAnimatorsEnabled()
        else Settings.Global.getFloat(context.contentResolver, Settings.Global.ANIMATOR_DURATION_SCALE, 1f) > 0f
    val animate = systemAllowsMotion && visible && stage.isAtLeast(Lifecycle.State.RESUMED)
    val logoScale by animateFloatAsState(
        targetValue = if (active) 1.12f else 0.94f,
        animationSpec = if (animate) tween(325, easing = CubicBezierEasing(0.22f, 1f, 0.36f, 1f)) else snap(),
        label = "proxy_logo_scale"
    )
    val reveal = animateFloatAsState(
        targetValue = if (ready) 1f else 0f,
        animationSpec = if (animate && ready) tween(320, easing = FastOutSlowInEasing) else snap(),
        label = "proxy_logo_reveal"
    )
    val image = painterResource(R.drawable.ic_telegram_logo)
    val grayscale = remember { ColorFilter.colorMatrix(ColorMatrix().apply { setToSaturation(0f) }) }
    val action = stringResource(if (active) R.string.flight_stop else R.string.flight_start)
    Box(
        modifier = Modifier.size(if (compactHeight) 126.dp else 220.dp)
            .clip(CircleShape)
            .clickable(enabled = enabled, role = Role.Button, onClickLabel = action, onClick = onPower),
        contentAlignment = Alignment.Center
    ) {
        Box(Modifier.size(if (compactHeight) 108.dp else 180.dp).graphicsLayer {
            scaleX = logoScale
            scaleY = logoScale
        }) {
            Image(image, action, Modifier.fillMaxSize(), colorFilter = grayscale, alpha = 0.52f)
            Image(image, null, Modifier.fillMaxSize().drawWithCache {
                val mask = Path()
                onDrawWithContent {
                    val amount = reveal.value
                    if (amount > 0f) {
                        if (amount >= 1f) {
                            drawContent()
                        } else {
                            // The symbol occupies the inner 28 units of the 32-unit vector.
                            val radius = size.minDimension * 0.445f * amount
                            mask.reset()
                            val extent = Offset(radius, radius)
                            mask.addOval(Rect(center - extent, center + extent))
                            clipPath(mask) { this@onDrawWithContent.drawContent() }
                        }
                    }
                }
            })
        }
    }
}
