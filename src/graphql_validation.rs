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
        let schema_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("graphql/github.graphql");
        let schema_str = fs::read_to_string(&schema_path)
            .unwrap_or_else(|e| panic!("Failed to read schema from {:?}: {}", schema_path, e));
        Schema::parse_and_validate(&schema_str, "github.graphql")
            .unwrap_or_else(|e| panic!("Failed to parse schema: {}", e))
    }

    fn validate_query(schema: &Valid<Schema>, query_file: &str) {
        let query_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("graphql/{}", query_file));
        let query_str = fs::read_to_string(&query_path)
            .unwrap_or_else(|e| panic!("Failed to read query from {:?}: {}", query_path, e));

        ExecutableDocument::parse_and_validate(schema, &query_str, query_file).unwrap_or_else(
            |e| {
                panic!(
                    "GraphQL validation failed for {}:\n{}",
                    query_file,
                    e.errors
                        .iter()
                        .map(|d| d.to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            },
        );
    }

    #[test]
    fn validate_fetch_threads_query() {
        let schema = load_schema();
        validate_query(&schema, "fetch_threads.graphql");
    }

    #[test]
    fn validate_fetch_remaining_comments_query() {
        let schema = load_schema();
        validate_query(&schema, "fetch_remaining_comments.graphql");
    }

    #[test]
    fn validate_fetch_comment_pr_info_query() {
        let schema = load_schema();
        validate_query(&schema, "fetch_comment_pr_info.graphql");
    }

    #[test]
    fn validate_add_reply_mutation() {
        let schema = load_schema();
        validate_query(&schema, "add_reply.graphql");
    }

    #[test]
    fn validate_delete_comment_mutation() {
        let schema = load_schema();
        validate_query(&schema, "delete_comment.graphql");
    }
}
