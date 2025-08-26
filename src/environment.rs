use std::collections::HashMap;

/// A type-safe wrapper for environment variables that provides clear semantics
/// for building and composing environment variable sets.
///
/// This type uses a `set` method instead of `insert` to emphasize the intent
/// of setting environment variables, making the code more readable and
/// self-documenting.
#[derive(Debug, Clone, Default)]
pub struct Environment(HashMap<String, String>);

impl Environment {
    /// Creates a new empty environment variable set.
    ///
    /// # Returns
    /// * `Self` - A new empty Environment instance
    pub fn new() -> Self {
        Self(HashMap::new())
    }
    
    /// Sets an environment variable in this environment set.
    ///
    /// This method uses the name `set` instead of `insert` to make the intent
    /// clear and provide better semantic meaning for environment variable operations.
    ///
    /// # Arguments
    /// * `key` - The environment variable name
    /// * `value` - The environment variable value
    ///
    /// # Examples
    /// ```
    /// use scinit::Environment;
    /// 
    /// let mut env = Environment::new();
    /// env.set("PATH", "/usr/bin");
    /// env.set("HOME", "/home/user");
    /// ```
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.0.insert(key.into(), value.into());
    }
    
    /// Extends this environment with variables from another environment.
    ///
    /// Variables in the `other` environment will overwrite variables with
    /// the same name in this environment.
    ///
    /// # Arguments
    /// * `other` - The environment to merge into this one
    ///
    /// # Examples
    /// ```
    /// use scinit::Environment;
    /// 
    /// let mut base_env = Environment::new();
    /// base_env.set("PATH", "/usr/bin");
    /// 
    /// let mut additional_env = Environment::new();
    /// additional_env.set("HOME", "/home/user");
    /// 
    /// base_env.extend(additional_env);
    /// ```
    pub fn extend(&mut self, other: Environment) {
        self.0.extend(other.0);
    }
    
    /// Consumes this Environment and returns the underlying HashMap.
    ///
    /// This method is useful when you need to pass the environment variables
    /// to APIs that expect a HashMap directly.
    ///
    /// # Returns
    /// * `HashMap<String, String>` - The underlying environment variable map
    pub fn into_inner(self) -> HashMap<String, String> {
        self.0
    }
    
    /// Gets the value of an environment variable.
    ///
    /// # Arguments
    /// * `key` - The environment variable name to look up
    ///
    /// # Returns
    /// * `Option<&String>` - The environment variable value, if present
    pub fn get(&self, key: &str) -> Option<&String> {
        self.0.get(key)
    }

    /// Returns true if the environment contains no variables.
    ///
    /// # Returns
    /// * `bool` - True if empty, false otherwise
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of environment variables.
    ///
    /// # Returns
    /// * `usize` - The number of environment variables
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl From<HashMap<String, String>> for Environment {
    /// Creates an Environment from a HashMap of strings.
    ///
    /// # Arguments
    /// * `map` - The HashMap to convert
    ///
    /// # Returns
    /// * `Self` - The Environment instance
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
}