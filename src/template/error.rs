use crate::{
    collection::{ChainId, ProfileId, RecipeId},
    http::{QueryError, RequestBuildError, RequestError},
    template::RECURSION_LIMIT,
    util::doc_link,
};
use nom::error::VerboseError;
use std::{env::VarError, io, path::PathBuf, string::FromUtf8Error};
use thiserror::Error;

/// An error while parsing a template. This is derived from a nom error
#[derive(Debug, Error)]
#[error("{0}")]
pub struct TemplateParseError(String);

impl TemplateParseError {
    /// Create a user-friendly parse error
    pub(super) fn new(template: &str, error: VerboseError<&str>) -> Self {
        Self(nom::error::convert_error(template, error))
    }
}

/// Any error that can occur during template rendering. The purpose of having a
/// structured error here (while the rest of the app just uses `anyhow`) is to
/// support localized error display in the UI, e.g. showing just one portion of
/// a string in red if that particular template key failed to render.
///
/// The error always holds owned data so it can be detached from the lifetime
/// of the template context. This requires a mild amount of cloning in error
/// cases, but those should be infrequent so it's fine.
///
/// These error messages are generally shown with additional parent context, so
/// they should be pretty brief.
#[derive(Debug, Error)]
#[cfg_attr(test, derive(PartialEq))]
pub enum TemplateError {
    /// Tried to load profile data with no profile selected
    #[error("No profile selected")]
    NoProfileSelected,

    /// Unknown profile ID
    #[error("Unknown profile `{profile_id}`")]
    ProfileUnknown { profile_id: ProfileId },

    /// A profile field key contained an unknown field
    #[error("Unknown field `{field}`")]
    FieldUnknown { field: String },

    /// An bubbled-up error from rendering a profile field value
    #[error("Rendering nested template for field `{field}`")]
    FieldNested {
        field: String,
        #[source]
        error: Box<Self>,
    },

    /// Too many templates!
    #[error(
        "Template recursion limit reached; cannot render more than \
        {RECURSION_LIMIT} nested templates"
    )]
    RecursionLimit,

    #[error("Resolving chain `{chain_id}`")]
    Chain {
        chain_id: ChainId,
        #[source]
        error: ChainError,
    },

    /// Variable either didn't exist or had non-unicode content
    #[error("Accessing environment variable `{variable}`")]
    EnvironmentVariable {
        variable: String,
        #[source]
        error: VarError,
    },
}

/// An error sub-type, for any error that occurs while resolving a chained
/// value. This is factored out because they all need to be paired with a chain
/// ID.
#[derive(Debug, Error)]
pub enum ChainError {
    /// Reference to a chain that doesn't exist
    #[error("Unknown chain: {_0}")]
    ChainUnknown(ChainId),

    /// Reference to a recipe that doesn't exist
    #[error("Unknown request recipe: {_0}")]
    RecipeUnknown(RecipeId),

    /// An error occurred accessing the persistence database. This error is
    /// generated by our code so we don't need any extra context.
    #[error(transparent)]
    Database(anyhow::Error),

    /// Chain source produced non-UTF-8 bytes
    #[error("Error decoding content as UTF-8")]
    InvalidUtf8 {
        #[source]
        error: FromUtf8Error,
    },

    /// The chain ID is valid, but the corresponding recipe has no successful
    /// response
    #[error("No response available")]
    NoResponse,

    /// Couldn't guess content type from request/file/etc. metadata
    #[error(
        "Selector cannot be applied; content type not provided and could not \
        be determined from metadata. See docs for supported content types: {}",
        doc_link("api/request_collection/content_type")
    )]
    UnknownContentType,

    /// Something bad happened while triggering a request dependency
    #[error("Triggering upstream recipe `{recipe_id}`")]
    Trigger {
        recipe_id: RecipeId,
        #[source]
        error: TriggeredRequestError,
    },

    /// Failed to parse the response body before applying a selector
    #[error("Parsing response")]
    ParseResponse {
        #[source]
        error: anyhow::Error,
    },

    /// Got either 0 or 2+ results for JSON path query. This is generated by
    /// internal code so we don't need extra context
    #[error(transparent)]
    Query(#[from] QueryError),

    /// User gave an empty list for the command
    #[error("No command given")]
    CommandMissing,

    /// Error executing an external command
    #[error("Executing command {command:?}")]
    Command {
        command: Vec<String>,
        #[source]
        error: io::Error,
    },

    /// Error opening/reading a file
    #[error("Reading file `{path}`")]
    File {
        path: PathBuf,
        #[source]
        error: io::Error,
    },

    /// Never got a response from the prompt channel. Do *not* store the
    /// `RecvError` here, because it provides useless extra output to the user.
    #[error("No response from prompt")]
    PromptNoResponse,

    /// A bubbled-error from rendering a nested template in the chain arguments
    #[error("Rendering nested template for field `{field}`")]
    Nested {
        /// Specific field that contained the error, to give the user context
        field: String,
        #[source]
        error: Box<TemplateError>,
    },

    /// Specified !header did not exist in the response
    #[error("Header `{header}` not in response")]
    MissingHeader { header: String },
}

/// Error occurred while trying to build/execute a triggered request
#[derive(Debug, Error)]
pub enum TriggeredRequestError {
    /// This render was invoked in a way that doesn't support automatic request
    /// execution. In some cases the user needs to explicitly opt in to enable
    /// it (e.g. with a CLI flag)
    #[error("Triggered request execution not allowed in this context")]
    NotAllowed,

    /// Tried to auto-execute a chained request but couldn't build it
    #[error(transparent)]
    Build(#[from] RequestBuildError),

    /// Chained request was triggered, sent and failed
    #[error(transparent)]
    Send(#[from] RequestError),
}

impl TemplateError {
    /// Does the given error have *any* error in its chain that contains
    /// [TriggeredRequestError::NotAllowed]? This makes it easy to attach
    /// additional error context.
    pub fn has_trigger_disabled_error(error: &anyhow::Error) -> bool {
        error.chain().any(|error| {
            matches!(
                error.downcast_ref(),
                Some(Self::Chain {
                    error: ChainError::Trigger {
                        error: TriggeredRequestError::NotAllowed,
                        ..
                    },
                    ..
                })
            )
        })
    }
}

/// Placeholder implementation to allow equality checks for *other*
/// `TemplateError` variants. This one is hard to do because `anyhow::Error`
/// doesn't impl `PartialEq`
#[cfg(test)]
impl PartialEq for ChainError {
    fn eq(&self, _: &Self) -> bool {
        unimplemented!("PartialEq for ChainError is hard to implement")
    }
}
