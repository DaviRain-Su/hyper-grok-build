use crate::terminal::tmux_probe;

pub type TmuxProbeResult<T> = tmux_probe::TmuxQueryResult<T>;

pub trait TmuxOptionQuery {
    fn show_option(&self, option: &str) -> TmuxProbeResult<String>;

    fn option_support(&self, option: &str) -> TmuxProbeResult<()>;

    fn control_mode(&self) -> TmuxProbeResult<bool>;
}

pub struct LiveTmuxProbe;

impl TmuxOptionQuery for LiveTmuxProbe {
    fn show_option(&self, option: &str) -> TmuxProbeResult<String> {
        tmux_probe::query_option(option)
    }

    fn option_support(&self, option: &str) -> TmuxProbeResult<()> {
        tmux_probe::query_option_support(option)
    }

    fn control_mode(&self) -> TmuxProbeResult<bool> {
        tmux_probe::query_control_mode()
    }
}
