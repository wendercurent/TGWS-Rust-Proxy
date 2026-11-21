# Privacy

TGWS Rust listens locally by default. Telegram explicitly sends proxy connections to the address and secret the user selects. The service uses the connection initialization to choose a data-center route and forwards encrypted Telegram traffic. Permitted routes can include a direct endpoint, the selected Cloudflare service, a configured Worker or the existing direct TCP fallback. Network operators and the chosen intermediary can observe transport metadata.

The Dev route log records session ordinals, data-center class, network epoch, first-party host where appropriate, failure stages, byte counts and timing. The release shows a local route summary and upload and download totals for the current proxy session. The support report contains version, Android API/ABI and coarse settings. Dev reports can include a local warning count. Log and proxy-link clipboard actions are user initiated. Review anything copied before sharing it.

The MTProto proxy link contains a secret. Android stores that secret in the app's private settings, which are subject to the device's Android backup settings. First-party route hints use a short-lived local network token in the private no-backup directory. They expire after 12 hours. The advanced route summary uses data-center and transport labels. This release candidate contains no automatic crash-report uploader or inherited update checker.
