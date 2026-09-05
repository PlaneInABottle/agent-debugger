//! Language adapter registry.
//!
//! Java/Python/Node are all live via their embedded bridges (JDI, pybridge,
//! nodebridge). This registry is scaffolding for a future trait-based
//! dispatch; the CLI currently dispatches in `session::spawn` instead.

// scaffolding for later phases
#![allow(dead_code)]

/// Languages the CLI supports. All four are live via embedded bridges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Java,
    Python,
    Node,
    Browser,
}

impl Language {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "java" => Some(Self::Java),
            "python" | "py" => Some(Self::Python),
            "node" | "js" | "javascript" | "typescript" | "ts" => Some(Self::Node),
            "browser" | "web" | "chrome" => Some(Self::Browser),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Java => "java",
            Self::Python => "python",
            Self::Node => "node",
            Self::Browser => "browser",
        }
    }

    /// Whether this binary can drive the language yet.
    pub fn is_available(self) -> bool {
        match self {
            Self::Java | Self::Python | Self::Node => true,
            // Browser session skeleton is live; debug core lands in B1.
            Self::Browser => false,
        }
    }
}

/// Per-language driver. Concrete adapters (JDI bridge, pybridge, nodebridge)
/// implement this in later phases; the CLI talks only to this trait.
pub trait Adapter {
    fn language(&self) -> Language;
    fn describe(&self) -> &'static str;
}

pub struct JavaAdapter;
pub struct PythonAdapter;
pub struct NodeAdapter;
pub struct BrowserAdapter;

impl Adapter for JavaAdapter {
    fn language(&self) -> Language {
        Language::Java
    }
    fn describe(&self) -> &'static str {
        "Java via JDI bridge (phase 1)"
    }
}

impl Adapter for PythonAdapter {
    fn language(&self) -> Language {
        Language::Python
    }
    fn describe(&self) -> &'static str {
        "Python via debugpy (planned)"
    }
}

impl Adapter for NodeAdapter {
    fn language(&self) -> Language {
        Language::Node
    }
    fn describe(&self) -> &'static str {
        "Node via nodebridge + CDP"
    }
}

impl Adapter for BrowserAdapter {
    fn language(&self) -> Language {
        Language::Browser
    }
    fn describe(&self) -> &'static str {
        "Browser tabs via browserbridge + CDP (B0 skeleton)"
    }
}
