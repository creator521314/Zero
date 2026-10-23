//! 全局 ID 类型。统一用 newtype 包裹，避免与 String 混用。

use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl $name {
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4().to_string())
            }

            /// 用既有字符串构造（用于按 id 反查，如「恢复某个历史任务」）。
            pub fn from_raw(raw: impl Into<String>) -> Self {
                Self(raw.into())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

id_type!(TaskId);
id_type!(SessionId);
id_type!(ToolCallId);
id_type!(ApprovalId);
