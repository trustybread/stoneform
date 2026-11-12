use url::Url;

use crate::{GameCodename, LaunchToken, Region};

/// The Battle.net web login that hands out launch tokens for `codename`.
///
/// The page is always the English one; it finishes with a redirect to
/// `http://localhost:0/?ST=<token>`, which [`token_from_callback`] reads.
pub fn login_url(region: Region, codename: GameCodename) -> Url {
    let mut url = Url::parse(&format!("https://{}.battle.net/login/en/", region.as_str()))
        .expect("a region code always makes a valid host");
    url.query_pairs_mut().append_pair("externalChallenge", "login").append_pair("app", codename.as_str());
    url
}

/// The token in the login page's final redirect, if `url` is that redirect.
pub fn token_from_callback(url: &str) -> Option<LaunchToken> {
    let url = Url::parse(url).ok()?;
    if url.host_str() != Some("localhost") {
        return None;
    }
    let (_, ticket) = url.query_pairs().find(|(name, _)| name == "ST")?;
    LaunchToken::parse(&ticket).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "US-0123456789abcdef0123456789abcdef-987654321";

    #[test]
    fn builds_the_wow_login_url() {
        assert_eq!(
            login_url(Region::Eu, GameCodename::WOW).as_str(),
            "https://eu.battle.net/login/en/?externalChallenge=login&app=WoW"
        );
        assert_eq!(login_url(Region::Kr, GameCodename::WOW).host_str(), Some("kr.battle.net"));
    }

    #[test]
    fn reads_the_token_from_the_final_redirect() {
        let token = token_from_callback(&format!("http://localhost:0/?ST={TOKEN}&accountId=1"));
        assert_eq!(token.map(|t| t.expose().to_owned()), Some(TOKEN.to_owned()));
    }

    #[test]
    fn ignores_everything_else() {
        assert!(token_from_callback(&format!("https://eu.battle.net/?ST={TOKEN}")).is_none());
        assert!(token_from_callback("http://localhost:0/?other=1").is_none());
        assert!(token_from_callback("http://localhost:0/?ST=not-a-token").is_none());
        assert!(token_from_callback("not a url").is_none());
    }
}
