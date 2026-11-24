use crate::ServerError;

/// Role bound to an API key (design doc section 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    Dev,
    Pm,
    Bot,
}

/// Permission class of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// read / list / search / compile / log / diff
    Read,
    /// write / delete / commit / ingest
    Write,
    /// branch / rewind / merge
    Branch,
    /// manage projects and keys
    Manage,
}

impl Role {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "admin" => Some(Self::Admin),
            "dev" => Some(Self::Dev),
            "pm" => Some(Self::Pm),
            "bot" => Some(Self::Bot),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Dev => "dev",
            Self::Pm => "pm",
            Self::Bot => "bot",
        }
    }

    pub fn allows(self, action: Action) -> bool {
        match self {
            Self::Admin => true,
            Self::Dev | Self::Pm => action != Action::Manage,
            Self::Bot => action == Action::Read,
        }
    }
}

/// Verified API key record. The plaintext key is never kept.
#[derive(Debug, Clone)]
pub struct ApiKey {
    pub id: i64,
    pub name: String,
    pub role: Role,
    /// `None` means all projects.
    pub projects: Option<Vec<String>>,
    pub expires_at_ms: Option<i64>,
    pub revoked: bool,
}

/// Checks that `key` may perform `action` on `project`.
pub fn authorize(key: &ApiKey, project: &str, action: Action) -> Result<(), ServerError> {
    if let Some(scope) = &key.projects {
        if !scope.iter().any(|p| p == project) {
            return Err(ServerError::Forbidden(format!(
                "key '{}' has no access to project '{project}'",
                key.name
            )));
        }
    }
    if !key.role.allows(action) {
        return Err(ServerError::Forbidden(format!(
            "role '{}' may not perform {action:?}",
            key.role.as_str()
        )));
    }
    Ok(())
}
