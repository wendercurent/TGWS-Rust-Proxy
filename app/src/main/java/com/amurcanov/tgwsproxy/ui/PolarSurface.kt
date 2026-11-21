package com.amurcanov.tgwsproxy.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.blur
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithCache
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.lerp
import androidx.compose.ui.graphics.luminance
import androidx.compose.ui.unit.dp

@Composable
internal fun PolarArtwork(modifier: Modifier = Modifier) {
    val colors = MaterialTheme.colorScheme
    val dark = colors.surface.luminance() < 0.22f
    Box(modifier.drawWithCache {
        val width = size.width
        val height = size.height
        fun point(x: Float, y: Float) = Offset(x * width, y * height)
        val sky = Brush.verticalGradient(
            0f to lerp(colors.background, colors.surfaceVariant, 0.32f),
            0.60f to colors.surface,
            1f to lerp(colors.background, colors.primaryContainer, 0.55f)
        )
        val light = Brush.radialGradient(
            colors = listOf(colors.secondary.copy(alpha = if (dark) 0.23f else 0.18f), Color.Transparent),
            center = point(0.84f, 0.26f),
            radius = width.coerceAtLeast(1f) * 0.76f
        )
        val ridge = Path().apply {
            moveTo(0f, height * 0.84f)
            cubicTo(width * 0.27f, height * 0.81f, width * 0.46f, height * 0.56f, width * 0.62f, height * 0.59f)
            cubicTo(width * 0.80f, height * 0.62f, width * 0.89f, height * 0.49f, width, height * 0.40f)
            lineTo(width, height)
            lineTo(0f, height)
            close()
        }
        val face = Brush.linearGradient(
            colors = listOf(colors.primary.copy(alpha = 0.16f), colors.background.copy(alpha = 0.15f)),
            start = point(0.87f, 0.39f), end = point(0.50f, 0.99f)
        )
        val shadedRidge = Path().apply {
            moveTo(0f, height * 0.91f)
            cubicTo(width * 0.23f, height * 0.89f, width * 0.53f, height * 0.74f, width * 0.72f, height * 0.78f)
            cubicTo(width * 0.88f, height * 0.82f, width * 0.93f, height * 0.64f, width, height * 0.61f)
            lineTo(width, height)
            lineTo(0f, height)
            close()
        }
        val edge = Path().apply {
            moveTo(width * 0.71f, height * 0.60f)
            cubicTo(width * 0.83f, height * 0.59f, width * 0.94f, height * 0.47f, width, height * 0.40f)
        }
        onDrawBehind {
            drawRect(sky)
            drawRect(light)
            drawPath(ridge, face)
            drawPath(shadedRidge, colors.background.copy(alpha = 0.38f))
            drawPath(edge, colors.primary.copy(alpha = if (dark) 0.20f else 0.13f), style = Stroke(0.75.dp.toPx()))
        }
    })
}

@Composable
internal fun FrostedHeader(modifier: Modifier = Modifier, content: @Composable BoxScope.() -> Unit) {
    val colors = MaterialTheme.colorScheme
    val shape = RoundedCornerShape(24.dp)
    Box(
        modifier
            .clip(shape)
            .background(colors.surface)
            .drawWithCache {
                val rim = Brush.verticalGradient(listOf(colors.primary.copy(alpha = 0.32f), colors.outlineVariant))
                val corner = androidx.compose.ui.geometry.CornerRadius(24.dp.toPx())
                onDrawWithContent {
                    drawContent()
                    drawRoundRect(rim, cornerRadius = corner, style = Stroke(1.dp.toPx()))
                }
            }
    ) {
        PolarArtwork(Modifier.matchParentSize().blur(14.dp))
        Box(Modifier.matchParentSize().background(colors.surface.copy(alpha = 0.58f)))
        content()
    }
}
