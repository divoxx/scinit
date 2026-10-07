use std::collections::HashMap;

/// Environment variables to set for a child process
#[derive(Debug, Clone, Default)]
pub struct Environment(HashMap<String, String>);

impl Environment {
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    /// Sets a variable, replacing any previous value
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.0.insert(key.into(), value.into());
    }

    /// Adds the variables of `other`, which win over existing ones
    pub fn extend(&mut self, other: Environment) {
        self.0.extend(other.0);
    }

    pub fn contains(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    pub fn into_inner(self) -> HashMap<String, String> {
        self.0
    }

    #[cfg(test)]
    pub fn get(&self, key: &str) -> Option<&String> {
        self.0.get(key)
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[cfg(test)]
    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }
}

impl From<HashMap<String, String>> for Environment {
    fn from(map: HashMap<String, String>) -> Self {
        Self(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_environment_creation() {
        let env = Environment::new();
        assert!(env.is_empty());
        assert_eq!(env.len(), 0);
    }

    #[test]
    fn test_environment_set() {
        let mut env = Environment::new();
        env.set("KEY1", "value1");
        env.set("KEY2", "value2");

        assert_eq!(env.len(), 2);
        assert_eq!(env.get("KEY1"), Some(&"value1".to_string()));
        assert_eq!(env.get("KEY2"), Some(&"value2".to_string()));
        assert_eq!(env.get("KEY3"), None);
    }

    #[test]
    fn test_environment_extend() {
        let mut env1 = Environment::new();
        env1.set("KEY1", "value1");

        let mut env2 = Environment::new();
        env2.set("KEY2", "value2");
        env2.set("KEY1", "overridden"); // Should overwrite env1's KEY1

        env1.extend(env2);

        assert_eq!(env1.len(), 2);
        assert_eq!(env1.get("KEY1"), Some(&"overridden".to_string()));
        assert_eq!(env1.get("KEY2"), Some(&"value2".to_string()));
    }

    #[test]
    fn test_environment_from_hashmap() {
        let mut map = HashMap::new();
        map.insert("KEY1".to_string(), "value1".to_string());
        map.insert("KEY2".to_string(), "value2".to_string());

        let env = Environment::from(map);

        assert_eq!(env.len(), 2);
        assert_eq!(env.get("KEY1"), Some(&"value1".to_string()));
        assert_eq!(env.get("KEY2"), Some(&"value2".to_string()));
    }

    #[test]
    fn test_environment_into_inner() {
        let mut env = Environment::new();
        env.set("KEY1", "value1");
        env.set("KEY2", "value2");

        let map = env.into_inner();

        assert_eq!(map.len(), 2);
        assert_eq!(map.get("KEY1"), Some(&"value1".to_string()));
        assert_eq!(map.get("KEY2"), Some(&"value2".to_string()));
    }

    #[test]
    fn test_environment_generic_types() {
        let mut env = Environment::new();

        // Test that set accepts different string-like types
        env.set("KEY1", "string_literal");
        env.set("KEY2".to_string(), "owned_string".to_string());
        env.set(format!("KEY{}", 3), format!("value{}", 3));

        assert_eq!(env.len(), 3);
        assert_eq!(env.get("KEY1"), Some(&"string_literal".to_string()));
        assert_eq!(env.get("KEY2"), Some(&"owned_string".to_string()));
        assert_eq!(env.get("KEY3"), Some(&"value3".to_string()));
    }

    #[test]
    fn test_environment_iter() {
        let mut env = Environment::new();
        env.set("PATH", "/usr/bin");
        env.set("HOME", "/home/user");
        env.set("SHELL", "/bin/bash");

        // Collect all key-value pairs from iterator
        let mut pairs: Vec<(&String, &String)> = env.iter().collect();
        pairs.sort(); // HashMap iteration order is not deterministic

        assert_eq!(pairs.len(), 3);

        // Find specific pairs (order may vary due to HashMap)
        let has_path = pairs.iter().any(|(k, v)| k.as_str() == "PATH" && v.as_str() == "/usr/bin");
        let has_home = pairs.iter().any(|(k, v)| k.as_str() == "HOME" && v.as_str() == "/home/user");
        let has_shell = pairs.iter().any(|(k, v)| k.as_str() == "SHELL" && v.as_str() == "/bin/bash");

        assert!(has_path);
        assert!(has_home);
        assert!(has_shell);

        // Test that iteration doesn't consume the environment
        assert_eq!(env.len(), 3);
        assert_eq!(env.get("PATH"), Some(&"/usr/bin".to_string()));
    }
}