mod error;
mod parse;
mod prompt;
mod render;

pub use error::{ChainError, TemplateError};
pub use prompt::{Prompt, PromptChannel, Prompter};

use crate::{
    collection::{ChainId, Collection, ProfileId},
    db::CollectionDatabase,
    http::HttpEngine,
    template::parse::{TemplateInputChunk, CHAIN_PREFIX, ENV_PREFIX},
};
use derive_more::{Deref, Display};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::{
    fmt::Debug,
    sync::{atomic::AtomicU8, Arc},
};

/// Maximum number of layers of nested templates
const RECURSION_LIMIT: u8 = 10;

/// A parsed template, which can contain raw and/or templated content. The
/// string is parsed during creation to identify template keys, hence the
/// immutability.
///
/// The original string is *not* stored. To recover the source string, use the
/// [Display] implementation.
///
/// Invariant: two templates with the same source string will have the same set
/// of chunks
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(into = "String", try_from = "String")]
pub struct Template {
    /// Pre-parsed chunks of the template. For raw chunks we store the
    /// presentation text (which is not necessarily the source text, as escape
    /// sequences will be eliminated). For keys, just store the needed
    /// metadata.
    chunks: Vec<TemplateInputChunk>,
}

/// A little container struct for all the data that the user can access via
/// templating. Unfortunately this has to own all data so templating can be
/// deferred into a task (tokio requires `'static` for spawned tasks). If this
/// becomes a bottleneck, we can `Arc` some stuff.
#[derive(Debug)]
pub struct TemplateContext {
    /// Entire request collection
    pub collection: Collection,
    /// ID of the profile whose data should be used for rendering. Generally
    /// the caller should check the ID is valid before passing it, to
    /// provide a better error to the user if not.
    pub selected_profile: Option<ProfileId>,
    /// HTTP engine used to executed triggered sub-requests. This should only
    /// be populated if you actually want to trigger requests! In some cases
    /// you want renders to be idempotent, in which case you should pass
    /// `None`.
    pub http_engine: Option<HttpEngine>,
    /// Needed for accessing response bodies for chaining
    pub database: CollectionDatabase,
    /// Additional key=value overrides passed directly from the user
    pub overrides: IndexMap<String, String>,
    /// A conduit to ask the user questions
    pub prompter: Box<dyn Prompter>,
    /// A count of how many templates have *already* been rendered with this
    /// context. This is used to prevent infinite recursion in templates. For
    /// all external calls, you can start this at 0.
    ///
    /// This tracks the *total* number of recursive calls in a render tree, not
    /// the number of *layers*. That means one template that renders 5 child
    /// templates is the same as a template that renders a single child 5
    /// times.
    pub recursion_count: AtomicU8,
}

impl Template {
    /// Create a new template from a raw string, without parsing it at all.
    /// Useful when importing from external formats where the string isn't
    /// expected to be a valid Slumber template
    pub fn raw(template: String) -> Template {
        let chunks = if template.is_empty() {
            vec![]
        } else {
            // This may seem too easy, but the hard part comes during
            // stringification, when we need to add backslashes to get the
            // string to parse correctly later
            vec![TemplateInputChunk::Raw(template.into())]
        };
        Self { chunks }
    }
}

#[cfg(test)]
impl From<&str> for Template {
    fn from(value: &str) -> Self {
        value.parse().unwrap()
    }
}

#[cfg(test)]
impl From<String> for Template {
    fn from(value: String) -> Self {
        value.as_str().into()
    }
}

/// An identifier that can be used in a template key. A valid identifier is
/// any string of one or more characters that contains only allowed characters,
/// as defined by [Self::is_char_allowed].
///
/// Construct via the [FromStr](std::str::FromStr) impl (in [parse] module)
#[derive(
    Clone,
    Debug,
    Deref,
    Default,
    Display,
    Eq,
    Hash,
    PartialEq,
    Serialize,
    Deserialize,
)]
pub struct Identifier(String);

/// A shortcut for creating identifiers from static strings. Since the string
/// is static we know it must be valid; panic if not.
impl From<&'static str> for Identifier {
    fn from(value: &'static str) -> Self {
        Self(value.parse().unwrap())
    }
}

/// A piece of a rendered template string. A collection of chunks collectively
/// constitutes a rendered string, and those chunks should be contiguous.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub enum TemplateChunk {
    /// Raw unprocessed text, i.e. something **outside** the `{{ }}`. This is
    /// stored in an `Arc` so we can reference the text in the parsed input
    /// without having to clone it.
    Raw(Arc<String>),
    /// Outcome of rendering a template key
    Rendered { value: Vec<u8>, sensitive: bool },
    /// An error occurred while rendering a template key
    Error(TemplateError),
}

#[cfg(test)]
impl TemplateChunk {
    /// Shorthand for creating a new raw chunk
    fn raw(value: &str) -> Self {
        Self::Raw(value.to_owned().into())
    }
}

/// A parsed template key. The variant of this determines how the key will be
/// resolved into a value.
///
/// This also serves as an enumeration of all possible value types. Once a key
/// is parsed, we know its value type and can dynamically dispatch for rendering
/// based on that.
///
/// The generic parameter defines *how* the key data is stored. Ideally we could
/// just store a `&str`, but that isn't possible when this is part of a
/// `Template`, because it would create a self-referential pointer. In that
/// case, we can store a `Span` which points back to its source in the template.
///
/// The `Display` impl here should return exactly what this was parsed from.
/// This is important for matching override keys during rendering.
#[derive(Clone, Debug, Display, PartialEq)]
enum TemplateKey {
    /// A plain field, which can come from the profile or an override
    Field(Identifier),
    /// A value from a predefined chain of another recipe
    #[display("{CHAIN_PREFIX}{_0}")]
    Chain(ChainId),
    /// A value pulled from the process environment
    /// DEPRECATED: To be removed in 2.0, replaced by !env chain source
    #[display("{ENV_PREFIX}{_0}")]
    Environment(Identifier),
}

#[cfg(test)]
impl crate::test_util::Factory for TemplateContext {
    fn factory(_: ()) -> Self {
        use crate::test_util::TestPrompter;
        Self {
            collection: Collection::default(),
            selected_profile: None,
            http_engine: None,
            database: CollectionDatabase::factory(()),
            overrides: IndexMap::new(),
            prompter: Box::<TestPrompter>::default(),
            recursion_count: 0.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        collection::{
            Chain, ChainOutputTrim, ChainRequestSection, ChainRequestTrigger,
            ChainSource, Profile, Recipe, RecipeId,
        },
        config::Config,
        http::{ContentType, Exchange, RequestRecord, ResponseRecord},
        test_util::{
            assert_err, by_id, header_map, temp_dir, Factory, TempDir,
            TestPrompter,
        },
        tui::test_util::EnvGuard,
    };
    use chrono::Utc;
    use indexmap::indexmap;
    use rstest::rstest;
    use serde_json::json;
    use std::time::Duration;
    use tokio::fs;

    /// Test overriding all key types, as well as missing keys
    #[tokio::test]
    async fn test_override() {
        let profile_data = indexmap! {"field1".into() => "field".into()};
        let overrides = indexmap! {
            "field1".into() => "override".into(),
            "chains.chain1".into() => "override".into(),
            "env.ENV1".into() => "override".into(),
            "override1".into() => "override".into(),
        };
        let profile = Profile {
            data: profile_data,
            ..Profile::factory(())
        };
        let profile_id = profile.id.clone();
        let chain = Chain {
            source: ChainSource::command(["echo", "chain"]),
            ..Chain::factory(())
        };
        let context = TemplateContext {
            collection: Collection {
                profiles: by_id([profile]),
                chains: by_id([chain]),
                ..Collection::factory(())
            },
            selected_profile: Some(profile_id),
            overrides,
            ..TemplateContext::factory(())
        };

        assert_eq!(
            render!("{{field1}}", context).unwrap(),
            "override".to_owned()
        );
        assert_eq!(
            render!("{{chains.chain1}}", context).unwrap(),
            "override".to_owned()
        );
        assert_eq!(
            render!("{{env.ENV1}}", context).unwrap(),
            "override".to_owned()
        );
        assert_eq!(
            render!("{{override1}}", context).unwrap(),
            "override".to_owned()
        );
    }

    /// Test that a field key renders correctly
    #[tokio::test]
    async fn test_field() {
        let context = profile_context(indexmap! {
            "user_id".into() => "1".into(),
            "group_id".into() => "3".into(),
            "recursive".into() => "user id: {{user_id}}".into(),
        });

        assert_eq!(&render!("", context).unwrap(), "");
        assert_eq!(&render!("plain", context).unwrap(), "plain");
        assert_eq!(&render!("{{recursive}}", context).unwrap(), "user id: 1");
        assert_eq!(
            // Test complex stitching. Emoji is important to test because the
            // stitching uses character indexes
            &render!("start {{user_id}} 🧡💛 {{group_id}} end", context)
                .unwrap(),
            "start 1 🧡💛 3 end"
        );
    }

    /// Potential error cases for a profile field
    #[rstest]
    #[case::unknown_field("{{onion_id}}", "Unknown field `onion_id`")]
    #[case::nested(
        "{{nested}}",
        "Rendering nested template for field `nested`: \
        Unknown field `onion_id`"
    )]
    #[case::recursion_limit(
        "{{recursive}}",
        "Template recursion limit reached"
    )]
    #[tokio::test]
    async fn test_field_error(#[case] template: &str, #[case] expected: &str) {
        let context = profile_context(indexmap! {
            "nested".into() => "{{onion_id}}".into(),
            "recursive".into() => "{{recursive}}".into(),
        });
        assert_err!(render!(template, context), expected);
    }

    /// Test success cases with chained responses
    #[rstest]
    #[case::no_selector(
        None,
        ChainRequestSection::Body,
        r#"{"array":[1,2],"bool":false,"number":6,"object":{"a":1},"string":"Hello World!"}"#
    )]
    #[case::string(Some("$.string"), ChainRequestSection::Body, "Hello World!")]
    #[case::number(Some("$.number"), ChainRequestSection::Body, "6")]
    #[case::bool(Some("$.bool"), ChainRequestSection::Body, "false")]
    #[case::array(Some("$.array"), ChainRequestSection::Body, "[1,2]")]
    #[case::object(Some("$.object"), ChainRequestSection::Body, "{\"a\":1}")]
    #[case::header(None, ChainRequestSection::Header("Token".into()), "Secret Value")]
    #[tokio::test]
    async fn test_chain_request(
        #[case] selector: Option<&str>,
        #[case] section: ChainRequestSection,
        #[case] expected_value: &str,
    ) {
        let recipe_id: RecipeId = "recipe1".into();
        let database = CollectionDatabase::factory(());
        let response_body = json!({
            "string": "Hello World!",
            "number": 6,
            "bool": false,
            "array": [1,2],
            "object": {"a": 1},
        });
        let response_headers =
            header_map(indexmap! {"Token" => "Secret Value"});
        let request = RequestRecord {
            recipe_id: recipe_id.clone(),
            ..RequestRecord::factory(())
        };
        let response = ResponseRecord {
            body: response_body.to_string().into_bytes().into(),
            headers: response_headers,
            ..ResponseRecord::factory(())
        };
        database
            .insert_exchange(&Exchange {
                request: request.into(),
                response: response.into(),
                ..Exchange::factory(())
            })
            .unwrap();
        let selector = selector.map(|s| s.parse().unwrap());
        let recipe = Recipe {
            id: recipe_id.clone(),
            ..Recipe::factory(())
        };
        let chain = Chain {
            source: ChainSource::Request {
                recipe: recipe_id.clone(),
                trigger: Default::default(),
                section,
            },
            selector,
            content_type: Some(ContentType::Json),
            ..Chain::factory(())
        };
        let context = TemplateContext {
            collection: Collection {
                recipes: by_id([recipe]).into(),
                chains: by_id([chain]),
                ..Collection::factory(())
            },
            database,
            ..TemplateContext::factory(())
        };

        assert_eq!(
            render!("{{chains.chain1}}", context).unwrap(),
            expected_value
        );
    }

    /// Test all possible error cases for chained requests. This covers all
    /// chain-specific error variants
    #[rstest]
    // Referenced a chain that doesn't exist
    #[case::unknown_chain(
        Chain {
            id: "unknown".into(),
            ..Chain::factory(())
        },
        None,
        None,
        "Unknown chain"
    )]
    // Chain references a recipe that's not in the collection
    #[case::unknown_recipe(
        Chain {
            source: ChainSource::Request {
                recipe: "unknown".into(),
                trigger: Default::default(),
                section: Default::default(),
            },
            ..Chain::factory(())
        },
        None,
        None,
        "Unknown request recipe",
    )]
    // Recipe exists but has no history in the DB
    #[case::no_response(
        Chain {
            source: ChainSource::Request {
                recipe: "recipe1".into(),
                trigger: Default::default(),
                section: Default::default(),
            },
            ..Chain::factory(())
        },
        Some("recipe1"),
        None,
        "No response available",
    )]
    // Subrequest can't be executed because triggers are disabled
    #[case::trigger_disabled(
        Chain {
            source: ChainSource::Request {
                recipe: "recipe1".into(),
                trigger: ChainRequestTrigger::Always,
                section: Default::default(),
            },
            ..Chain::factory(())
        },
        Some("recipe1"),
        None,
        "Triggered request execution not allowed in this context",
    )]
    // Response doesn't include a hint to its content type
    #[case::no_content_type(
        Chain {
            source: ChainSource::Request {
                recipe: "recipe1".into(),
                trigger: Default::default(),
                section: Default::default(),
            },
            selector: Some("$.message".parse().unwrap()),
            ..Chain::factory(())
        },
        Some("recipe1"),
        Some(Exchange {
            response: ResponseRecord {
                body: "not json!".into(),
                ..ResponseRecord::factory(())
            }.into(),
            ..Exchange::factory(())
        }),
        "content type not provided",
    )]
    // Response can't be parsed according to the content type we gave
    #[case::parse_response(
        Chain {
            source: ChainSource::Request {
                recipe: "recipe1".into(),
                trigger: Default::default(),
                section: Default::default(),
            },
            selector: Some("$.message".parse().unwrap()),
            content_type: Some(ContentType::Json),
            ..Chain::factory(())
        },
        Some("recipe1"),
        Some(Exchange {
            response: ResponseRecord {
                body: "not json!".into(),
                ..ResponseRecord::factory(())
            }.into(),
            ..Exchange::factory(())
        }),
        "Parsing response: expected ident at line 1 column 2",
    )]
    // Query returned multiple results
    #[case::query_multiple_results(
        Chain {
            source: ChainSource::Request {
                recipe: "recipe1".into(),
                trigger: Default::default(),
                section:Default::default()
            },
            selector: Some("$.*".parse().unwrap()),
            content_type: Some(ContentType::Json),
            ..Chain::factory(())
        },
        Some("recipe1"),
        Some(Exchange {
            response: ResponseRecord {
                body: "[1, 2]".into(),
                ..ResponseRecord::factory(())
            }.into(),
            ..Exchange::factory(())
        }),
        "Expected exactly one result",
    )]
    #[tokio::test]
    async fn test_chain_request_error(
        #[case] chain: Chain,
        // ID of a recipe to add to the collection
        #[case] recipe_id: Option<&str>,
        // Optional request/response data to store in the database
        #[case] exchange: Option<Exchange>,
        #[case] expected_error: &str,
    ) {
        let database = CollectionDatabase::factory(());

        let mut recipes = IndexMap::new();
        if let Some(recipe_id) = recipe_id {
            let recipe_id: RecipeId = recipe_id.into();
            recipes.insert(
                recipe_id.clone(),
                Recipe {
                    id: recipe_id,
                    ..Recipe::factory(())
                },
            );
        }

        // Insert exchange into DB
        if let Some(exchange) = exchange {
            database.insert_exchange(&exchange).unwrap();
        }

        let context = TemplateContext {
            collection: Collection {
                recipes: recipes.into(),
                chains: by_id([chain]),
                ..Collection::factory(())
            },
            database,
            ..TemplateContext::factory(())
        };

        assert_err!(render!("{{chains.chain1}}", context), expected_error);
    }

    /// Test triggered sub-requests. We expect all of these *to trigger*
    #[rstest]
    #[case::no_history(ChainRequestTrigger::NoHistory, None)]
    #[case::expire_empty(
        ChainRequestTrigger::Expire(Duration::from_secs(0)),
        None
    )]
    #[case::expire_with_duration(
        ChainRequestTrigger::Expire(Duration::from_secs(60)),
        Some(Exchange {
            end_time: Utc::now() - Duration::from_secs(100),
            ..Exchange::factory(())})
    )]
    #[case::always_no_history(ChainRequestTrigger::Always, None)]
    #[case::always_with_history(
        ChainRequestTrigger::Always,
        Some(Exchange::factory(()))
    )]
    #[tokio::test]
    async fn test_triggered_request(
        #[case] trigger: ChainRequestTrigger,
        // Optional request data to store in the database
        #[case] exchange: Option<Exchange>,
    ) {
        let database = CollectionDatabase::factory(());

        // Set up DB
        if let Some(exchange) = exchange {
            database.insert_exchange(&exchange).unwrap();
        }

        // Mock HTTP response
        let mut server = mockito::Server::new_async().await;
        let url = server.url();
        let mock = server
            .mock("GET", "/get")
            .with_status(200)
            .with_body("hello!")
            .create_async()
            .await;

        let recipe = Recipe {
            url: format!("{url}/get").into(),
            ..Recipe::factory(())
        };
        let chain = Chain {
            source: ChainSource::Request {
                recipe: recipe.id.clone(),
                trigger,
                section: Default::default(),
            },
            ..Chain::factory(())
        };
        let http_engine = HttpEngine::new(&Config::default());
        let context = TemplateContext {
            collection: Collection {
                recipes: by_id([recipe]).into(),
                chains: by_id([chain]),
                ..Collection::factory(())
            },
            http_engine: Some(http_engine),
            database,
            ..TemplateContext::factory(())
        };

        assert_eq!(render!("{{chains.chain1}}", context).unwrap(), "hello!");

        mock.assert();
    }

    /// Test success with chained command
    #[rstest]
    #[case::with_stdin(&["tail"], Some("hello!"), "hello!")]
    #[case::raw_command(&["echo", "-n", "hello!"], None, "hello!")]
    #[tokio::test]
    async fn test_chain_command(
        #[case] command: &[&str],
        #[case] stdin: Option<&str>,
        #[case] expected: &str,
    ) {
        let source = ChainSource::Command {
            command: command.iter().copied().map(Template::from).collect(),
            stdin: stdin.map(Template::from),
        };
        let chain = Chain {
            source,
            ..Chain::factory(())
        };
        let context = TemplateContext {
            collection: Collection {
                chains: by_id([chain]),
                ..Collection::factory(())
            },
            ..TemplateContext::factory(())
        };

        assert_eq!(render!("{{chains.chain1}}", context).unwrap(), expected);
    }

    /// Test trimmed chained command
    #[rstest]
    #[case::no_trim(ChainOutputTrim::None, "   hello!   ")]
    #[case::trim_start(ChainOutputTrim::Start, "hello!   ")]
    #[case::trim_end(ChainOutputTrim::End, "   hello!")]
    #[case::trim_both(ChainOutputTrim::Both, "hello!")]
    #[tokio::test]
    async fn test_chain_output_trim(
        #[case] trim: ChainOutputTrim,
        #[case] expected: &str,
    ) {
        let chain = Chain {
            source: ChainSource::command(["echo", "-n", "   hello!   "]),
            trim,
            ..Chain::factory(())
        };
        let context = TemplateContext {
            collection: Collection {
                chains: by_id([chain]),
                ..Collection::factory(())
            },
            ..TemplateContext::factory(())
        };

        assert_eq!(render!("{{chains.chain1}}", context).unwrap(), expected);
    }

    /// Test failure with chained command
    #[rstest]
    #[case::no_command(&[], None, "No command given")]
    #[case::unknown_command(
        &["totally not a program"], None, "No such file or directory"
    )]
    #[case::command_error(
        &["head", "/dev/random"], None, "invalid utf-8 sequence"
    )]
    #[case::stdin_error(
        &["tail"],
        Some("{{chains.stdin}}"),
        "Resolving chain `chain1`: Rendering nested template for field `stdin`: \
         Resolving chain `stdin`: Unknown chain: stdin"
    )]
    #[tokio::test]
    async fn test_chain_command_error(
        #[case] command: &[&str],
        #[case] stdin: Option<&str>,
        #[case] expected_error: &str,
    ) {
        let source = ChainSource::Command {
            command: command.iter().copied().map(Template::from).collect(),
            stdin: stdin.map(Template::from),
        };
        let chain = Chain {
            source,
            ..Chain::factory(())
        };
        let context = TemplateContext {
            collection: Collection {
                chains: by_id([chain]),
                ..Collection::factory(())
            },
            ..TemplateContext::factory(())
        };

        assert_err!(render!("{{chains.chain1}}", context), expected_error);
    }

    /// Test success with a chained environment variable
    #[rstest]
    #[case::present(Some("test!"), "test!")]
    #[case::missing(None, "")]
    #[tokio::test]
    async fn test_chain_environment(
        #[case] env_value: Option<&str>,
        #[case] expected: &str,
    ) {
        let source = ChainSource::Environment {
            variable: "TEST".into(),
        };
        let chain = Chain {
            source,
            ..Chain::factory(())
        };
        let context = TemplateContext {
            collection: Collection {
                chains: by_id([chain]),
                ..Collection::factory(())
            },
            ..TemplateContext::factory(())
        };
        // This prevents tests from competing for environment variables, and
        // isolates us from the external env
        let result = {
            let _guard = EnvGuard::lock([("TEST", env_value)]);
            render!("{{chains.chain1}}", context)
        };
        assert_eq!(result.unwrap(), expected);
    }

    /// Test success with chained file
    #[rstest]
    #[tokio::test]
    async fn test_chain_file(temp_dir: TempDir) {
        // Create a temp file that we'll read from
        let path = temp_dir.join("stuff.txt");
        fs::write(&path, "hello!").await.unwrap();
        // Sanity check to debug race condition
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "hello!");
        let path: Template = path.to_str().unwrap().into();

        let chain = Chain {
            source: ChainSource::File { path: path.clone() },
            ..Chain::factory(())
        };
        let context = TemplateContext {
            collection: Collection {
                chains: by_id([chain]),
                ..Collection::factory(())
            },
            ..TemplateContext::factory(())
        };

        assert_eq!(
            render!("{{chains.chain1}}", context).unwrap(),
            "hello!",
            "{path:?}"
        );
    }

    /// Test failure with chained file
    #[tokio::test]
    async fn test_chain_file_error() {
        let chain = Chain {
            source: ChainSource::File {
                path: "not-real".into(),
            },
            ..Chain::factory(())
        };
        let context = TemplateContext {
            collection: Collection {
                chains: by_id([chain]),
                ..Collection::factory(())
            },
            ..TemplateContext::factory(())
        };

        assert_err!(
            render!("{{chains.chain1}}", context),
            "Reading file `not-real`"
        );
    }

    #[tokio::test]
    async fn test_chain_prompt() {
        let chain = Chain {
            source: ChainSource::Prompt {
                message: Some("password".into()),
                default: Some("default".into()),
            },
            ..Chain::factory(())
        };

        // Test value from prompter
        let mut context = TemplateContext {
            collection: Collection {
                chains: by_id([chain]),
                ..Collection::factory(())
            },

            prompter: Box::new(TestPrompter::new(Some("hello!"))),
            ..TemplateContext::factory(())
        };
        assert_eq!(render!("{{chains.chain1}}", context).unwrap(), "hello!");

        // Test default value
        context.prompter = Box::new(TestPrompter::new::<String>(None));
        assert_eq!(render!("{{chains.chain1}}", context).unwrap(), "default");
    }

    /// Prompting gone wrong
    #[tokio::test]
    async fn test_chain_prompt_error() {
        let chain = Chain {
            source: ChainSource::Prompt {
                message: Some("password".into()),
                default: None,
            },
            ..Chain::factory(())
        };
        let context = TemplateContext {
            collection: Collection {
                chains: by_id([chain]),
                ..Collection::factory(())
            },
            // Prompter gives no response
            prompter: Box::new(TestPrompter::new::<String>(None)),
            ..TemplateContext::factory(())
        };

        assert_err!(
            render!("{{chains.chain1}}", context),
            "No response from prompt"
        );
    }

    /// Values marked sensitive should have that flag set in the rendered output
    #[tokio::test]
    async fn test_chain_sensitive() {
        let chain = Chain {
            source: ChainSource::Prompt {
                message: Some("password".into()),
                default: None,
            },
            sensitive: true,
            ..Chain::factory(())
        };
        let context = TemplateContext {
            collection: Collection {
                chains: by_id([chain]),
                ..Collection::factory(())
            },
            // Prompter gives no response
            prompter: Box::new(TestPrompter::new(Some("hello!"))),
            ..TemplateContext::factory(())
        };
        assert_eq!(
            Template::from("{{chains.chain1}}")
                .render_chunks(&context)
                .await,
            vec![TemplateChunk::Rendered {
                value: "hello!".into(),
                sensitive: true
            }]
        );
    }

    /// Test linking two chains together. This example is contribed because the
    /// command could just read the file itself, but don't worry about it it's
    /// just a test.
    #[rstest]
    #[tokio::test]
    async fn test_chain_nested(temp_dir: TempDir) {
        // Chain 1 - file
        let path = temp_dir.join("stuff.txt");
        fs::write(&path, "hello!").await.unwrap();
        let path: Template = path.to_str().unwrap().into();
        let file_chain = Chain {
            id: "file".into(),
            source: ChainSource::File { path },
            ..Chain::factory(())
        };

        // Chain 2 - command
        let command_chain = Chain {
            id: "command".into(),
            source: ChainSource::command([
                "echo",
                "-n",
                "answer: {{chains.file}}",
            ]),
            ..Chain::factory(())
        };

        let context = TemplateContext {
            collection: Collection {
                chains: by_id([file_chain, command_chain]),
                ..Collection::factory(())
            },
            ..TemplateContext::factory(())
        };
        assert_eq!(
            render!("{{chains.command}}", context).unwrap(),
            "answer: hello!"
        );
    }

    /// Test when an error occurs in a nested chain
    #[tokio::test]
    async fn test_chain_nested_error() {
        // Chain 1 - file
        let file_chain = Chain {
            id: "file".into(),
            source: ChainSource::File {
                path: "bogus.txt".into(),
            },

            ..Chain::factory(())
        };

        // Chain 2 - command
        let command_chain = Chain {
            id: "command".into(),
            source: ChainSource::command([
                "echo",
                "-n",
                "answer: {{chains.file}}",
            ]),
            ..Chain::factory(())
        };

        let context = TemplateContext {
            collection: Collection {
                chains: by_id([file_chain, command_chain]),
                ..Collection::factory(())
            },
            ..TemplateContext::factory(())
        };
        assert_err!(
            render!("{{chains.command}}", context),
            "Rendering nested template for field `command[2]`: \
            Resolving chain `file`: Reading file `bogus.txt`: \
            No such file or directory"
        );
    }

    #[rstest]
    #[case::present(Some("test!"), "test!")]
    #[case::missing(None, "")]
    #[tokio::test]
    async fn test_environment_success(
        #[case] env_value: Option<&str>,
        #[case] expected: &str,
    ) {
        let context = TemplateContext::factory(());
        // This prevents tests from competing for environ environment variables,
        // and isolates us from the external env
        let result = {
            let _guard = EnvGuard::lock([("TEST", env_value)]);
            render!("{{env.TEST}}", context)
        };
        assert_eq!(result.unwrap(), expected);
    }

    /// Test rendering non-UTF-8 data
    #[tokio::test]
    async fn test_render_binary() {
        let chain = Chain {
            source: ChainSource::command(["echo", "-n", "-e", r#"\xc3\x28"#]),
            ..Chain::factory(())
        };
        let context = TemplateContext {
            collection: Collection {
                chains: by_id([chain]),
                ..Collection::factory(())
            },
            ..TemplateContext::factory(())
        };

        assert_eq!(
            Template::from("{{chains.chain1}}")
                .render(&context)
                .await
                .unwrap(),
            b"\xc3\x28"
        );
    }

    /// Test rendering non-UTF-8 data to string returns an error
    #[tokio::test]
    async fn test_render_invalid_utf8() {
        let chain = Chain {
            source: ChainSource::command(["echo", "-n", "-e", r#"\xc3\x28"#]),
            ..Chain::factory(())
        };
        let context = TemplateContext {
            collection: Collection {
                chains: by_id([chain]),
                ..Collection::factory(())
            },
            ..TemplateContext::factory(())
        };

        assert_err!(render!("{{chains.chain1}}", context), "invalid utf-8");
    }

    /// Test rendering into individual chunks with complex unicode
    #[tokio::test]
    async fn test_render_chunks() {
        let context =
            profile_context(indexmap! { "user_id".into() => "🧡💛".into() });

        let chunks =
            Template::from("intro {{user_id}} 💚💙💜 {{unknown}} outro")
                .render_chunks(&context)
                .await;
        assert_eq!(
            chunks,
            vec![
                TemplateChunk::raw("intro "),
                TemplateChunk::Rendered {
                    value: "🧡💛".into(),
                    sensitive: false
                },
                // Each emoji is 4 bytes
                TemplateChunk::raw(" 💚💙💜 "),
                TemplateChunk::Error(TemplateError::FieldUnknown {
                    field: "unknown".into()
                }),
                TemplateChunk::raw(" outro"),
            ]
        );
    }

    /// Tested rendering a template with escaped keys, which should be treated
    /// as raw text
    #[tokio::test]
    async fn test_render_escaped() {
        let context =
            profile_context(indexmap! { "user_id".into() => "user1".into() });
        let template = r#"user: {{user_id}} escaped: \{{user_id}}"#;
        assert_eq!(
            render!(template, context).unwrap(),
            "user: user1 escaped: {{user_id}}"
        );
    }

    /// Build a template context that only has simple profile data
    fn profile_context(data: IndexMap<String, Template>) -> TemplateContext {
        let profile = Profile {
            data,
            ..Profile::factory(())
        };
        let profile_id = profile.id.clone();
        TemplateContext {
            collection: Collection {
                profiles: by_id([profile]),
                ..Collection::factory(())
            },
            selected_profile: Some(profile_id),
            ..TemplateContext::factory(())
        }
    }

    /// Helper for rendering a template to a string
    macro_rules! render {
        ($template:expr, $context:expr) => {
            Template::from($template).render_string(&$context).await
        };
    }
    use render;
}
