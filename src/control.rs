/// Parameters are validated at the RPC boundary, then encoded exactly as PowerView.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    ToggleBeeper,
    SelfTest,
    TimedTest {
        minutes: u16,
    },
    DischargeTest,
    CancelTest,
    Shutdown {
        delay_seconds: u16,
        restore_after_minutes: Option<u16>,
    },
    CancelShutdown,
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Self::ToggleBeeper => "toggle_beeper",
            Self::SelfTest => "self_test",
            Self::TimedTest { .. } => "timed_battery_test",
            Self::DischargeTest => "discharge_test",
            Self::CancelTest => "cancel_battery_test",
            Self::Shutdown {
                restore_after_minutes: Some(_),
                ..
            } => "shutdown_restore",
            Self::Shutdown { .. } => "shutdown",
            Self::CancelShutdown => "cancel_shutdown",
        }
    }

    pub fn request(&self) -> [u8; 7] {
        let (code, first, second) = match self {
            Self::ToggleBeeper => (b'M', u16::MAX, u16::MAX),
            Self::SelfTest => (b'T', 16, 0),
            Self::TimedTest { minutes } => (b'T', minutes * 100, 0),
            Self::DischargeTest => (b'L', u16::MAX, u16::MAX),
            Self::CancelTest => (b'D', u16::MAX, u16::MAX),
            Self::Shutdown {
                delay_seconds,
                restore_after_minutes,
            } => (
                if restore_after_minutes.is_some() {
                    b'R'
                } else {
                    b'S'
                },
                *delay_seconds,
                restore_after_minutes.unwrap_or(u16::MAX),
            ),
            Self::CancelShutdown => (b'C', u16::MAX, u16::MAX),
        };
        let [a, b] = first.to_be_bytes();
        let [c, d] = second.to_be_bytes();
        let checksum = [code, a, b, c, d]
            .into_iter()
            .fold(0u8, u8::wrapping_add)
            .wrapping_neg();
        [code, a, b, c, d, checksum, b'\r']
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_control_frames_have_explicit_timer_units() {
        assert_eq!(Command::SelfTest.request(), [b'T', 0, 16, 0, 0, 0x9c, 13]);
        assert_eq!(
            Command::TimedTest { minutes: 1 }.request(),
            [b'T', 0, 100, 0, 0, 0x48, 13]
        );
        assert_eq!(
            Command::ToggleBeeper.request(),
            [b'M', 255, 255, 255, 255, 0xb7, 13]
        );
        assert_eq!(
            Command::Shutdown {
                delay_seconds: 300,
                restore_after_minutes: Some(60)
            }
            .request(),
            [b'R', 1, 44, 0, 60, 0x45, 13]
        );
        assert_eq!(
            Command::CancelShutdown.request(),
            [b'C', 255, 255, 255, 255, 0xc1, 13]
        );
    }
}
