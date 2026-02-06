// GraphQL query validation using apollo-compiler.
// This module validates our GraphQL queries against the GitHub schema at test time.
// If a query is invalid, the test will fail with a descriptive error.

#[cfg(test)]
mod tests {
    use apollo_compiler::validation::Valid;
    use apollo_compiler::{ExecutableDocument, Schema};
    use std::fs;
    use std::path::Path;

    fn load_schema() -> Valid<Schema> {
        let schema_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("graphql/schema/github.graphql");
        let schema_str = fs::read_to_string(&schema_path)
            .unwrap_or_else(|e| panic!("Failed to read schema from {:?}: {}", schema_path, e));
        Schema::parse_and_validate(&schema_str, "github.graphql")
            .unwrap_or_else(|e| panic!("Failed to parse schema: {}", e))
    }

    #[test]
    fn validate_all_operations() {
        let schema = load_schema();
        let operations_dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("graphql/operation");

        let entries: Vec<_> = fs::read_dir(&operations_dir)
            .unwrap_or_else(|e| panic!("Failed to read operations directory: {}", e))
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.path().extension().map_or(false, |ext| ext == "graphql")
            })
            .collect();

        assert!(!entries.is_empty(), "No .graphql files found in operations directory");

        let mut failures = Vec::new();
        for entry in entries {
            let path = entry.path();
            let filename = path.file_name().unwrap().to_string_lossy().to_string();
            let query_str = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("Failed to read {:?}: {}", path, e));

            if let Err(e) = ExecutableDocument::parse_and_validate(&schema, &query_str, &path) {
                let errors: Vec<_> = e.errors.iter().map(|d| d.to_string()).collect();
                failures.push(format!("{}:\n  {}", filename, errors.join("\n  ")));
            }
        }

        if !failures.is_empty() {
            panic!(
                "GraphQL validation failed for {} operation(s):\n\n{}",
                failures.len(),
                failures.join("\n\n")
            );
        }
    }
}
