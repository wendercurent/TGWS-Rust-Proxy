# Changelog

## 1.0.0 · ARM64 release candidate

- Put appearance controls and the three palettes in the floating theme button.
- Add launcher icon previews with accessible selection for Fold, Relay T and Network 101.
- Show an Android chronometer and update the session totals whenever the displayed traffic or connection status changes. Confirm a sustained outage before showing it.
- Keep the log viewer and experimental routing controls in Dev. The release keeps connection settings, route diagnostics and the sanitized support report.

- Separate the listener, recent Telegram traffic and route health on the connection screen and notification.
- Refresh the Compose controls, settings groups, log viewer and the independent fork identity. Mask log exports and hide the key by default.
- Preserve the tested native transport in the review APK. The default build number is 124. Production requires the maintainer's package identity and private signing key.

- Prefer a responding route per Android network and signed Telegram data center. Use bounded retry and revalidation for alternatives.
- Keep the upstream connection bound to each session after the first relay init write. Reject a transport established across a network epoch change before sending init.
- Add Russian and English route diagnostics, controlled retry, and an option to use the configured route order.
- Refresh the default Material 3 palette and clarify original project credits. Disable inherited release discovery and reduce the details included in a copied support report.
- Build ARM64 review artifacts from pinned source dependencies. Keep final package ownership and release signing gated by owner configuration.
- Real Telegram media remains intermittent. See the route stability and device verification reports.
