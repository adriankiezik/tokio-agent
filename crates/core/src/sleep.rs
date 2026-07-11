pub(crate) struct SleepInhibitor {
    _guard: Option<keepawake::KeepAwake>,
}

impl SleepInhibitor {
    pub(crate) fn acquire() -> Self {
        let guard = keepawake::Builder::default()
            .idle(true)
            .reason("tokio-agent is running an active turn")
            .app_name("tokio-agent")
            .app_reverse_domain("com.github.adriankiezik.tokio-agent")
            .create()
            .map_err(|error| {
                tracing::warn!(%error, "failed to prevent idle system sleep");
            })
            .ok();

        Self { _guard: guard }
    }
}
