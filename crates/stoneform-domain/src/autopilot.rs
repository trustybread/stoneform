/// What StoneForm knows when it decides what to do next.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutopilotFacts {
    pub signed_in: bool,
    pub installed: bool,
    /// The game is already running from the install folder.
    pub game_running: bool,
    /// The Battle.net app or its Agent, which also writes game files.
    pub battle_net_running: bool,
    /// Blizzard serves a newer build than the installed one. False when
    /// offline: the installed build is launched, and the game says if it's too old.
    pub update_available: bool,
}

/// The next thing the autopilot does. Only `SignIn` and `Install` wait for the
/// user, and they come first, so the user can walk away during the download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutopilotStep {
    /// The game runs already: show it instead of starting a second one.
    BringGameForward,
    SignIn,
    /// Installing or updating writes game files, which Battle.net may be writing too.
    WaitForBattleNet,
    Install,
    Update,
    Launch,
}

pub fn next_step(facts: &AutopilotFacts) -> AutopilotStep {
    if facts.game_running {
        AutopilotStep::BringGameForward
    } else if !facts.signed_in {
        AutopilotStep::SignIn
    } else if (!facts.installed || facts.update_available) && facts.battle_net_running {
        AutopilotStep::WaitForBattleNet
    } else if !facts.installed {
        AutopilotStep::Install
    } else if facts.update_available {
        AutopilotStep::Update
    } else {
        AutopilotStep::Launch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(signed_in: bool, installed: bool, game_running: bool, battle_net_running: bool) -> AutopilotStep {
        next_step(&AutopilotFacts { signed_in, installed, game_running, battle_net_running, update_available: false })
    }

    fn update_step(battle_net_running: bool, game_running: bool) -> AutopilotStep {
        let facts = AutopilotFacts {
            signed_in: true,
            installed: true,
            game_running,
            battle_net_running,
            update_available: true,
        };
        next_step(&facts)
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
    fn updates_before_launching_but_never_under_a_running_game() {
        assert_eq!(update_step(false, false), AutopilotStep::Update);
        assert_eq!(update_step(true, false), AutopilotStep::WaitForBattleNet);
        assert_eq!(update_step(false, true), AutopilotStep::BringGameForward);
    }

    #[test]
    fn waits_for_battle_net_only_before_writing_files() {
        assert_eq!(step(true, false, false, true), AutopilotStep::WaitForBattleNet);
        assert_eq!(step(true, true, false, true), AutopilotStep::Launch);
    }
}
