use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! secret_string {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Returns the secret text. Call only where the value is actually needed.
            pub fn expose(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($name), "(<redacted>)"))
            }
        }
    };
}

secret_string!(
    /// A password. `Debug` never prints it, so request structs are safe to log.
    Password
);
secret_string!(
    /// A bearer token identifying a signed-in session. `Debug` never prints it.
    SessionToken
);
secret_string!(
    /// The code that lets someone join a workspace (see `lazy_domain::workspace`). It is shown
    /// once, when the invitation is created, and the server keeps only a hash. `Debug` never
    /// prints it.
    InviteCode
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_is_redacted() {
        assert_eq!(
            format!("{:?}", Password::new("hunter2")),
            "Password(<redacted>)"
        );
        assert!(!format!("{:?}", SessionToken::new("abc123")).contains("abc123"));
        assert!(!format!("{:?}", InviteCode::new("abc123")).contains("abc123"));
    }
}
