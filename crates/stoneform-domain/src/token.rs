use std::fmt;

/// A Battle.net account id: the numeric suffix of a [`LaunchToken`].
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountId(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a Battle.net account id is a non-empty run of digits")]
pub struct ParseAccountIdError;

impl AccountId {
    pub fn parse(value: &str) -> Result<Self, ParseAccountIdError> {
        if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
            Ok(Self(value.to_owned()))
        } else {
            Err(ParseAccountIdError)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AccountId({})", self.0)
    }
}

/// A Battle.net login ticket, such as `EU-0123…cdef-123456789`.
///
/// It is what the web login hands out and what the game reads on start-up. It
/// is a secret: `Debug` shows only a redacted form, there is no `Display`, and
/// [`LaunchToken::expose`] is for storing and handing it off, never for logs.
#[derive(Clone, PartialEq, Eq)]
pub struct LaunchToken {
    value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseTokenError {
    #[error("a launch token has three parts separated by '-'")]
    Shape,
    #[error("a launch token starts with a two-letter login server")]
    Server,
    #[error("a launch token's ticket is 32 hex digits")]
    Ticket,
    #[error("a launch token ends with the account id")]
    Account,
}

impl LaunchToken {
    /// Parses `XX-<32 hex digits>-<account id>`.
    ///
    /// The prefix is the login server that issued the ticket, not the play
    /// region: accounts that play in Europe can get `US-` tokens.
    pub fn parse(value: &str) -> Result<Self, ParseTokenError> {
        let mut parts = value.split('-');
        let (Some(server), Some(ticket), Some(account), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(ParseTokenError::Shape);
        };
        if server.len() != 2 || !server.bytes().all(|b| b.is_ascii_alphabetic()) {
            return Err(ParseTokenError::Server);
        }
        if ticket.len() != 32 || !ticket.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ParseTokenError::Ticket);
        }
        AccountId::parse(account).map_err(|_| ParseTokenError::Account)?;
        Ok(Self { value: value.to_owned() })
    }

    /// The Battle.net account this token signs in.
    pub fn account_id(&self) -> AccountId {
        let suffix = self.value.rsplit('-').next().unwrap_or_default();
        AccountId(suffix.to_owned())
    }

    /// The secret value, for the secret store and the hand-off to the game only.
    pub fn expose(&self) -> &str {
        &self.value
    }

    /// Safe to log: the login server and the length, nothing else.
    pub fn redacted(&self) -> String {
        format!("{}…({} chars)", &self.value[..3], self.value.len())
    }
}

impl fmt::Debug for LaunchToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LaunchToken({})", self.redacted())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICKET: &str = "0123456789abcdef0123456789ABCDEF";

    fn token(text: &str) -> Result<LaunchToken, ParseTokenError> {
        LaunchToken::parse(text)
    }

    #[test]
    fn parses_a_well_formed_token() {
        let parsed = token(&format!("EU-{TICKET}-123456789")).unwrap();
        assert_eq!(parsed.account_id(), AccountId::parse("123456789").unwrap());
        assert_eq!(parsed.expose(), format!("EU-{TICKET}-123456789"));
    }

    #[test]
    fn rejects_malformed_tokens() {
        assert_eq!(token(""), Err(ParseTokenError::Shape));
        assert_eq!(token(&format!("EU-{TICKET}")), Err(ParseTokenError::Shape));
        assert_eq!(token(&format!("EU-{TICKET}-1-2")), Err(ParseTokenError::Shape));
        assert_eq!(token(&format!("EUR-{TICKET}-1")), Err(ParseTokenError::Server));
        assert_eq!(token(&format!("E1-{TICKET}-1")), Err(ParseTokenError::Server));
        assert_eq!(token("EU-0123-1"), Err(ParseTokenError::Ticket));
        assert_eq!(token(&format!("EU-{}-1", "g".repeat(32))), Err(ParseTokenError::Ticket));
        assert_eq!(token(&format!("EU-{TICKET}-")), Err(ParseTokenError::Account));
        assert_eq!(token(&format!("EU-{TICKET}-12a")), Err(ParseTokenError::Account));
    }

    #[test]
    fn debug_never_shows_the_secret() {
        let parsed = token(&format!("US-{TICKET}-42")).unwrap();
        let shown = format!("{parsed:?}");
        assert!(!shown.contains(TICKET), "{shown}");
        assert_eq!(shown, "LaunchToken(US-…(38 chars))");
    }

    #[test]
    fn account_ids_are_digits_only() {
        assert!(AccountId::parse("123").is_ok());
        assert!(AccountId::parse("").is_err());
        assert!(AccountId::parse("12 3").is_err());
        assert!(AccountId::parse("-1").is_err());
    }
}
