use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// One table is the only source of the variant, its stable name and its keycap label.
macro_rules! keys {
    ($($variant:ident = $name:literal, $label:literal;)*) => {
        /// A physical key of a full ANSI keyboard, numpad included. Modifiers are sided.
        ///
        /// Serde and IPC use [`Key::as_str`]; a released name never changes.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(u8)]
        pub enum Key {
            $($variant,)*
        }

        impl Key {
            /// Every key, in declaration order (`Key::ALL[k as usize] == k`).
            pub const ALL: [Key; [$($name),*].len()] = [$(Key::$variant),*];

            /// The stable name used in config, serde and IPC.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Key::$variant => $name,)*
                }
            }

            /// Text printed on the keycap in the settings keyboard.
            pub const fn label(self) -> &'static str {
                match self {
                    $(Key::$variant => $label,)*
                }
            }
        }
    };
}

keys! {
    Esc = "Esc", "Esc";
    F1 = "F1", "F1"; F2 = "F2", "F2"; F3 = "F3", "F3"; F4 = "F4", "F4";
    F5 = "F5", "F5"; F6 = "F6", "F6"; F7 = "F7", "F7"; F8 = "F8", "F8";
    F9 = "F9", "F9"; F10 = "F10", "F10"; F11 = "F11", "F11"; F12 = "F12", "F12";
    F13 = "F13", "F13"; F14 = "F14", "F14"; F15 = "F15", "F15"; F16 = "F16", "F16";
    F17 = "F17", "F17"; F18 = "F18", "F18"; F19 = "F19", "F19"; F20 = "F20", "F20";
    F21 = "F21", "F21"; F22 = "F22", "F22"; F23 = "F23", "F23"; F24 = "F24", "F24";
    Backquote = "Backquote", "`";
    Digit1 = "1", "1"; Digit2 = "2", "2"; Digit3 = "3", "3"; Digit4 = "4", "4";
    Digit5 = "5", "5"; Digit6 = "6", "6"; Digit7 = "7", "7"; Digit8 = "8", "8";
    Digit9 = "9", "9"; Digit0 = "0", "0";
    Minus = "Minus", "-"; Equal = "Equal", "="; Backspace = "Backspace", "Backspace";
    Tab = "Tab", "Tab";
    Q = "Q", "Q"; W = "W", "W"; E = "E", "E"; R = "R", "R"; T = "T", "T";
    Y = "Y", "Y"; U = "U", "U"; I = "I", "I"; O = "O", "O"; P = "P", "P";
    BracketLeft = "BracketLeft", "["; BracketRight = "BracketRight", "]";
    Backslash = "Backslash", "\\";
    CapsLock = "CapsLock", "Caps Lock";
    A = "A", "A"; S = "S", "S"; D = "D", "D"; F = "F", "F"; G = "G", "G";
    H = "H", "H"; J = "J", "J"; K = "K", "K"; L = "L", "L";
    Semicolon = "Semicolon", ";"; Quote = "Quote", "'"; Enter = "Enter", "Enter";
    LShift = "LShift", "Shift";
    Z = "Z", "Z"; X = "X", "X"; C = "C", "C"; V = "V", "V"; B = "B", "B";
    N = "N", "N"; M = "M", "M";
    Comma = "Comma", ","; Period = "Period", "."; Slash = "Slash", "/";
    RShift = "RShift", "Shift";
    LCtrl = "LCtrl", "Ctrl"; LWin = "LWin", "Win"; LAlt = "LAlt", "Alt";
    Space = "Space", "Space";
    RAlt = "RAlt", "Alt"; RWin = "RWin", "Win"; Menu = "Menu", "Menu"; RCtrl = "RCtrl", "Ctrl";
    PrintScreen = "PrintScreen", "PrtSc"; ScrollLock = "ScrollLock", "ScrLk";
    Pause = "Pause", "Pause";
    Insert = "Insert", "Ins"; Home = "Home", "Home"; PageUp = "PageUp", "PgUp";
    Delete = "Delete", "Del"; End = "End", "End"; PageDown = "PageDown", "PgDn";
    Up = "Up", "\u{2191}"; Left = "Left", "\u{2190}"; Down = "Down", "\u{2193}";
    Right = "Right", "\u{2192}";
    NumLock = "NumLock", "Num Lock"; NumDivide = "NumDivide", "/";
    NumMultiply = "NumMultiply", "*"; NumSubtract = "NumSubtract", "-";
    Num7 = "Num7", "7"; Num8 = "Num8", "8"; Num9 = "Num9", "9"; NumAdd = "NumAdd", "+";
    Num4 = "Num4", "4"; Num5 = "Num5", "5"; Num6 = "Num6", "6";
    Num1 = "Num1", "1"; Num2 = "Num2", "2"; Num3 = "Num3", "3"; NumEnter = "NumEnter", "Enter";
    Num0 = "Num0", "0"; NumDecimal = "NumDecimal", ".";
}

/// A side-agnostic modifier, as a chord names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Modifier {
    Ctrl,
    Shift,
    Alt,
    /// Cmd on macOS.
    Win,
}

impl Modifier {
    pub(super) const ALL: [Modifier; 4] = [Self::Ctrl, Self::Shift, Self::Alt, Self::Win];

    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Ctrl => "Ctrl",
            Self::Shift => "Shift",
            Self::Alt => "Alt",
            Self::Win => "Win",
        }
    }

    /// Left key first: the paste chord sends the left one.
    pub(super) const fn sides(self) -> [Key; 2] {
        match self {
            Self::Ctrl => [Key::LCtrl, Key::RCtrl],
            Self::Shift => [Key::LShift, Key::RShift],
            Self::Alt => [Key::LAlt, Key::RAlt],
            Self::Win => [Key::LWin, Key::RWin],
        }
    }
}

impl Key {
    /// Looks a key up by its [`Key::as_str`] name, ignoring ASCII case.
    pub fn from_name(name: &str) -> Option<Key> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str().eq_ignore_ascii_case(name))
    }

    pub(super) const fn modifier(self) -> Option<Modifier> {
        match self {
            Key::LCtrl | Key::RCtrl => Some(Modifier::Ctrl),
            Key::LShift | Key::RShift => Some(Modifier::Shift),
            Key::LAlt | Key::RAlt => Some(Modifier::Alt),
            Key::LWin | Key::RWin => Some(Modifier::Win),
            _ => None,
        }
    }
}

impl Serialize for Key {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let name = String::deserialize(d)?;
        Key::from_name(&name).ok_or_else(|| de::Error::custom(format!("unknown key {name:?}")))
    }
}
