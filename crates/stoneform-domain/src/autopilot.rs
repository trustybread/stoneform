/// What StoneForm knows when it decides what to do next.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutopilotFacts {
    pub signed_in: bool,
    pub installed: bool,
    /// The game is already running from the install folder.
    pub game_running: bool,
    /// The Battle.net app or its Agent, which also writes game files.
    pub battle_net_running: bool,
}

/// The next thing the autopilot does. Only `SignIn` and `Install` wait for the
/// user, and they come first, so the user can walk away during the download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutopilotStep {
    /// The game runs already: show it instead of starting a second one.
    BringGameForward,
    SignIn,
    /// Installing writes game files, which Battle.net may be writing too.
    WaitForBattleNet,
    Install,
    Launch,
}

pub fn next_step(facts: &AutopilotFacts) -> AutopilotStep {
    if facts.game_running {
        AutopilotStep::BringGameForward
    } else if !facts.signed_in {
        AutopilotStep::SignIn
    } else if !facts.installed && facts.battle_net_running {
        AutopilotStep::WaitForBattleNet
    } else if !facts.installed {
        AutopilotStep::Install
    } else {
        AutopilotStep::Launch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(signed_in: bool, installed: bool, game_running: bool, battle_net_running: bool) -> AutopilotStep {
        next_step(&AutopilotFacts { signed_in, installed, game_running, battle_net_running })
    }

    #[test]
    fn signs_in_installs_then_launches() {
        assert_eq!(step(false, false, false, false), AutopilotStep::SignIn);
        assert_eq!(step(true, false, false, false), AutopilotStep::Install);
        assert_eq!(step(true, true, false, false), AutopilotStep::Launch);
        assert_eq!(step(false, true, false, false), AutopilotStep::SignIn);
    }

    #[test]
    fn a_running_game_comes_first() {
        assert_eq!(step(false, false, true, true), AutopilotStep::BringGameForward);
        assert_eq!(step(true, true, true, false), AutopilotStep::BringGameForward);
    }

    #[test]
    fn waits_for_battle_net_only_before_writing_files() {
        assert_eq!(step(true, false, false, true), AutopilotStep::WaitForBattleNet);
        assert_eq!(step(true, true, false, true), AutopilotStep::Launch);
    }
}
