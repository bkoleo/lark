use clap::Parser;

#[derive(Parser, Debug, Clone, Default)]
#[command(name = "handy", about = "Handy - Speech to Text")]
pub struct CliArgs {
    /// Start with the main window hidden
    #[arg(long)]
    pub start_hidden: bool,

    /// Disable the system tray icon
    #[arg(long)]
    pub no_tray: bool,

    /// Toggle transcription on/off (sent to running instance)
    #[arg(long)]
    pub toggle_transcription: bool,

    /// Toggle transcription with post-processing on/off (sent to running instance)
    #[arg(long)]
    pub toggle_post_process: bool,

    /// Cancel the current operation (sent to running instance)
    #[arg(long)]
    pub cancel: bool,

    /// Toggle meeting recording on/off (sent to running instance, macOS only)
    #[arg(long)]
    pub toggle_meeting: bool,

    /// Pick the mic for the running meeting recording by device name, same
    /// as choosing it on the recording pill; "auto" clears the pick (sent
    /// to running instance, macOS only)
    #[arg(long, value_name = "DEVICE")]
    pub set_meeting_mic: Option<String>,

    /// Re-transcribe a saved meeting from its WAV tracks, overwriting the
    /// transcript. Takes either track or the .md (sent to running instance,
    /// macOS only)
    #[arg(long, value_name = "PATH")]
    pub retranscribe_meeting: Option<String>,

    /// How many minutes of a detected call to hold in memory so a late
    /// Record still catches the start of it. 0 turns the rewind off (sent to
    /// running instance, macOS only)
    #[arg(long, value_name = "MINUTES")]
    pub meeting_rewind: Option<u32>,

    /// Enable debug mode with verbose logging
    #[arg(long)]
    pub debug: bool,
}
