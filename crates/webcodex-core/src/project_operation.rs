//! Shared declarative scope for portable project lifecycle operations.

use serde::{Deserialize, Serialize};

pub(crate) const PROJECT_OPERATION_SCOPE_MAX_PACKAGES: usize = 8;
pub(crate) const PROJECT_OPERATION_SCOPE_MAX_PACKAGE_BYTES: usize = 256;

/// Portable dependency-resolution policy for bounded project operations.
///
/// `Locked` means the adapter must not repair dependency selection by updating
/// the ecosystem's project-level resolution state. Network access is a separate
/// policy dimension and is intentionally not controlled here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectDependencyMode {
    Locked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectDependencyPolicy {
    pub mode: ProjectDependencyMode,
}

/// Portable project-unit package selection shared by build and validation.
///
/// Exactly one selector is valid: bounded explicit `packages`, or
/// `all_packages=true`. The all-packages intent stays ecosystem-neutral here;
/// adapters translate it only after Runner-owned Project authority is proven.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectOperationScope {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub all_packages: bool,
}

#[derive(schemars::JsonSchema)]
#[serde(untagged)]
enum ProjectOperationScopeSchema {
    Packages(ProjectOperationPackagesScopeSchema),
    AllPackages(ProjectOperationAllPackagesScopeSchema),
}

#[derive(schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProjectOperationPackagesScopeSchema {
    #[schemars(length(min = 1, max = 8))]
    #[schemars(inner(length(min = 1, max = 256)))]
    packages: Vec<String>,
}

#[derive(schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProjectOperationAllPackagesScopeSchema {
    #[schemars(schema_with = "all_packages_true_schema")]
    all_packages: bool,
}

fn all_packages_true_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type": "boolean", "const": true})
}

impl schemars::JsonSchema for ProjectOperationScope {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ProjectOperationScope".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <ProjectOperationScopeSchema as schemars::JsonSchema>::json_schema(generator)
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectOperationScopeError {
    Selection,
    PackageCount,
    InvalidPackage,
}

impl ProjectOperationScope {
    pub(crate) fn validate(&self) -> Result<(), ProjectOperationScopeError> {
        match (self.packages.is_empty(), self.all_packages) {
            (true, true) => return Ok(()),
            (false, false) => {}
            _ => return Err(ProjectOperationScopeError::Selection),
        }
        if self.packages.len() > PROJECT_OPERATION_SCOPE_MAX_PACKAGES {
            return Err(ProjectOperationScopeError::PackageCount);
        }
        if self.packages.iter().any(|package| {
            package.is_empty()
                || package.len() > PROJECT_OPERATION_SCOPE_MAX_PACKAGE_BYTES
                || package.chars().any(char::is_control)
        }) {
            return Err(ProjectOperationScopeError::InvalidPackage);
        }
        Ok(())
    }

    pub fn explicit_packages(&self) -> Option<&[String]> {
        (!self.packages.is_empty()).then_some(self.packages.as_slice())
    }

    pub const fn selects_all_packages(&self) -> bool {
        self.all_packages
    }
}
