use rand::seq::IndexedRandom;

pub const ADJECTIVES: &[&str] = &[
    "blue", "green", "red", "gold", "silver", "swift", "brave", "calm", "wild", "bold", "keen",
    "wise", "silent", "fierce", "noble", "cosmic", "crystal", "electric", "frozen", "iron",
    "lunar", "mystic", "northern", "radiant", "shadow", "ember", "frost", "storm", "stellar",
    "amber",
];

pub const NOUNS: &[&str] = &[
    "castle", "forest", "river", "mountain", "eagle", "wolf", "phoenix", "falcon", "hawk", "raven",
    "tiger", "bear", "beacon", "forge", "gateway", "kernel", "oracle", "sentinel", "tower", "fox",
    "owl", "panther", "viper", "crane", "otter", "lynx", "cedar", "oak", "pine", "reef",
];

pub fn generate_workspace_name() -> String {
    let mut rng = rand::rng();
    let adj = ADJECTIVES.choose(&mut rng).unwrap_or(&"swift");
    let noun = NOUNS.choose(&mut rng).unwrap_or(&"agent");
    format!("{adj}-{noun}")
}
