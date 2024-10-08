use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::function::{FunctionConfig, FunctionConfigChat, FunctionConfigJson};
use crate::jsonschema_util::JSONSchemaFromPath;
use crate::minijinja_util::TemplateConfig;
use crate::model::ModelConfig;
use crate::tool::{
    ImplicitToolConfig, StaticToolConfig, ToolCallConfig, ToolChoice, ToolConfig,
    IMPLICIT_TOOL_NAME,
};
use crate::variant::VariantConfig;

#[derive(Debug, Default)]
pub struct Config<'c> {
    pub gateway: GatewayConfig,
    pub models: HashMap<String, ModelConfig>, // model name => model config
    pub functions: HashMap<String, FunctionConfig>, // function name => function config
    pub metrics: HashMap<String, MetricConfig>, // metric name => metric config
    pub tools: HashMap<String, StaticToolConfig>, // tool name => tool config
    pub templates: TemplateConfig<'c>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    pub bind_address: Option<std::net::SocketAddr>,
    #[serde(default)]
    pub disable_observability: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricConfig {
    pub r#type: MetricConfigType,
    pub optimize: MetricConfigOptimize,
    pub level: MetricConfigLevel,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricConfigType {
    Boolean,
    Float,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricConfigOptimize {
    Min,
    Max,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricConfigLevel {
    Inference,
    Episode,
}

impl std::fmt::Display for MetricConfigLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let serialized = serde_json::to_string(self).map_err(|_| std::fmt::Error)?;
        // Remove the quotes around the string
        write!(f, "{}", serialized.trim_matches('"'))
    }
}

impl<'c> Config<'c> {
    pub fn load() -> Result<Config<'c>, Error> {
        let config_path = UninitializedConfig::get_config_path();
        let config_table = UninitializedConfig::read_toml_config(&config_path)?;
        let base_path = match PathBuf::from(&config_path).parent() {
            Some(base_path) => base_path.to_path_buf(),
            None => {
                return Err(Error::Config {
                    message: format!(
                        "Failed to get parent directory of config file: {config_path}"
                    ),
                })
            }
        };
        let config = Self::load_from_toml(config_table, base_path)?;
        Ok(config)
    }

    fn load_from_toml(table: toml::Table, base_path: PathBuf) -> Result<Config<'c>, Error> {
        let config = UninitializedConfig::try_from(table)?;

        let gateway = config.gateway.unwrap_or_default();

        let templates = TemplateConfig::new();

        let functions = config
            .functions
            .into_iter()
            .map(|(name, config)| config.load(&base_path).map(|c| (name, c)))
            .collect::<Result<HashMap<String, FunctionConfig>, Error>>()?;

        let tools = config
            .tools
            .into_iter()
            .map(|(name, config)| config.load(&base_path, name.clone()).map(|c| (name, c)))
            .collect::<Result<HashMap<String, StaticToolConfig>, Error>>()?;

        let mut config = Config {
            gateway,
            models: config.models,
            functions,
            metrics: config.metrics,
            tools,
            templates,
        };

        // Initialize the templates
        let template_paths = config.get_templates(&base_path);
        config.templates.initialize(template_paths)?;

        // Validate the config
        config.validate()?;

        Ok(config)
    }

    /// Validate the config
    fn validate(&self) -> Result<(), Error> {
        // Validate each model
        for (model_name, model) in &self.models {
            // Ensure that the model has at least one provider
            if model.routing.is_empty() {
                return Err(Error::Config {
                    message: format!(
                        "Invalid Config: `models.{model_name}`: `routing` must not be empty"
                    ),
                });
            }

            // Ensure that routing entries are unique and exist as keys in providers
            let mut seen_providers = std::collections::HashSet::new();
            for provider in &model.routing {
                if !seen_providers.insert(provider) {
                    return Err(Error::Config {
                        message: format!("Invalid Config: `models.{model_name}.routing`: duplicate entry `{provider}`"),
                    });
                }

                if !model.providers.contains_key(provider) {
                    return Err(Error::Config {
                        message: format!("Invalid Config: `models.{model_name}`: `routing` contains entry `{provider}` that does not exist in `providers`"),
                    });
                }
            }

            // Validate each provider
            for provider_name in model.providers.keys() {
                if !seen_providers.contains(provider_name) {
                    return Err(Error::Config {
                        message: format!("Invalid Config: `models.{model_name}`: Provider `{provider_name}` is not listed in `routing`"),
                    });
                }
            }
        }

        // Validate each function
        for (function_name, function) in &self.functions {
            // Validate each variant
            for (variant_name, variant) in function.variants() {
                // Ensure that the weight is non-negative
                if variant.weight() < 0.0 {
                    return Err(Error::Config {
                        message: format!("Invalid Config: `functions.{function_name}.variants.{variant_name}.weight`: must be non-negative"),
                    });
                }

                // Ensure that the variant type is correct
                match function {
                    FunctionConfig::Chat(function) => {
                        // Check that the variant type matches the function type
                        if !matches!(variant, VariantConfig::ChatCompletion(_)) {
                            return Err(Error::Config {
                                message: format!("Invalid Config: `functions.{function_name}.variants.{variant_name}`: variant type must be `chat_completion`"),
                            });
                        }

                        // Check that system schema <=> system template
                        match (&function.system_schema, &variant.system_template()) {
                            (None, Some(system_template)) => {
                                // If the template is specified but there is no schema, we need to check that the template has the required variables
                                let system_template_name =
                                    system_template.to_str().ok_or(Error::InvalidTemplatePath)?;

                                if self
                                    .templates
                                    .template_needs_variables(system_template_name)?
                                {
                                    return Err(Error::Config {
                                        message: format!("Invalid Config: `functions.{function_name}.variants.{variant_name}`: `system_schema` is required when `system_template` is specified"),
                                    });
                                }
                            }
                            (Some(_), None) => {
                                return Err(Error::Config {
                                    message: format!("Invalid Config: `functions.{function_name}.variants.{variant_name}`: `system_template` is required when `system_schema` is specified"),
                                });
                            }
                            _ => {}
                        }

                        // Check that user schema <=> user template
                        match (&function.user_schema, &variant.user_template()) {
                            (None, Some(user_template)) => {
                                // If the template is specified but there is no schema, we need to check that the template has the required variables
                                let user_template_name =
                                    user_template.to_str().ok_or(Error::InvalidTemplatePath)?;

                                if self
                                    .templates
                                    .template_needs_variables(user_template_name)?
                                {
                                    return Err(Error::Config {
                                        message: format!("Invalid Config: `functions.{function_name}.variants.{variant_name}`: `user_schema` is required when `user_template` is specified"),
                                    });
                                }
                            }
                            (Some(_), None) => {
                                return Err(Error::Config {
                                    message: format!("Invalid Config: `functions.{function_name}.variants.{variant_name}`: `user_template` is required when `user_schema` is specified"),
                                });
                            }
                            _ => {}
                        }

                        // Check that assistant schema <=> assistant template
                        match (&function.assistant_schema, &variant.assistant_template()) {
                            (None, Some(assistant_template)) => {
                                // If the template is specified but there is no schema, we need to check that the template has the required variables
                                let assistant_template_name = assistant_template
                                    .to_str()
                                    .ok_or(Error::InvalidTemplatePath)?;

                                if self
                                    .templates
                                    .template_needs_variables(assistant_template_name)?
                                {
                                    return Err(Error::Config {
                                        message: format!("Invalid Config: `functions.{function_name}.variants.{variant_name}`: `assistant_schema` is required when `assistant_template` is specified"),
                                    });
                                }
                            }
                            (Some(_), None) => {
                                return Err(Error::Config {
                                    message: format!("Invalid Config: `functions.{function_name}.variants.{variant_name}`: `assistant_template` is required when `assistant_schema` is specified"),
                                });
                            }
                            _ => {}
                        }

                        // Check that tools that are specified are present
                        for tool in function.tools.iter() {
                            self.get_tool(tool).map_err(|_| {
                                Error::Config {
                                    message: format!("Invalid Config: `functions.{function_name}.tools`: tool `{tool}` is not present in the config"),
                                }
                            })?;
                        }
                    }
                    FunctionConfig::Json(function) => {
                        // Check that the variant type matches the function type
                        if !matches!(variant, VariantConfig::ChatCompletion(_)) {
                            return Err(Error::Config {
                                message: format!("Invalid Config: `functions.{function_name}.variants.{variant_name}`: variant type must be `chat_completion`"),
                            });
                        }

                        // Check that system schema <=> system template
                        match (&function.system_schema, &variant.system_template()) {
                            (None, Some(system_template)) => {
                                // If the template is specified but there is no schema, we need to check that the template has the required variables
                                let system_template_name =
                                    system_template.to_str().ok_or(Error::InvalidTemplatePath)?;

                                if self
                                    .templates
                                    .template_needs_variables(system_template_name)?
                                {
                                    return Err(Error::Config {
                                        message: format!("Invalid Config: `functions.{function_name}.variants.{variant_name}`: `system_schema` is required when `system_template` is specified"),
                                    });
                                }
                            }
                            (Some(_), None) => {
                                return Err(Error::Config {
                                    message: format!("Invalid Config: `functions.{function_name}.variants.{variant_name}`: `system_template` is required when `system_schema` is specified"),
                                });
                            }
                            _ => {}
                        }

                        // Check that user schema <=> user template
                        match (&function.user_schema, &variant.user_template()) {
                            (None, Some(user_template)) => {
                                // If the template is specified but there is no schema, we need to check that the template has the required variables
                                let user_template_name =
                                    user_template.to_str().ok_or(Error::InvalidTemplatePath)?;

                                if self
                                    .templates
                                    .template_needs_variables(user_template_name)?
                                {
                                    return Err(Error::Config {
                                        message: format!("Invalid Config: `functions.{function_name}.variants.{variant_name}`: `user_schema` is required when `user_template` is specified"),
                                    });
                                }
                            }
                            (Some(_), None) => {
                                return Err(Error::Config {
                                    message: format!("Invalid Config: `functions.{function_name}.variants.{variant_name}`: `user_template` is required when `user_schema` is specified"),
                                });
                            }
                            _ => {}
                        }

                        // Check that assistant schema <=> assistant template
                        match (&function.assistant_schema, &variant.assistant_template()) {
                            (None, Some(assistant_template)) => {
                                // If the template is specified but there is no schema, we need to check that the template has the required variables
                                let assistant_template_name = assistant_template
                                    .to_str()
                                    .ok_or(Error::InvalidTemplatePath)?;

                                if self
                                    .templates
                                    .template_needs_variables(assistant_template_name)?
                                {
                                    return Err(Error::Config {
                                        message: format!("Invalid Config: `functions.{function_name}.variants.{variant_name}`: `assistant_schema` is required when `assistant_template` is specified"),
                                    });
                                }
                            }
                            (Some(_), None) => {
                                return Err(Error::Config {
                                    message: format!("Invalid Config: `functions.{function_name}.variants.{variant_name}`: `assistant_template` is required when `assistant_schema` is specified"),
                                });
                            }
                            _ => {}
                        }
                    }
                }

                // Validate variant-specific config
                match variant {
                    VariantConfig::ChatCompletion(params) => {
                        // Ensure that the model exists
                        if !self.models.contains_key(&params.model) {
                            return Err(Error::Config {
                                message: format!("Invalid Config: `functions.{function_name}.variants.{variant_name}`: `model` must be a valid model name"),
                            });
                        }
                    }
                }
            }
        }

        // Ensure that no metrics are named "comment" or "demonstration"
        for metric_name in self.metrics.keys() {
            if metric_name == "comment" || metric_name == "demonstration" {
                return Err(Error::Config {
                    message: format!(
                        "Invalid Config: Metric name '{}' is reserved and cannot be used",
                        metric_name
                    ),
                });
            }
        }

        Ok(())
    }

    /// Get a function by name
    pub fn get_function<'a>(&'a self, function_name: &str) -> Result<&'a FunctionConfig, Error> {
        self.functions
            .get(function_name)
            .ok_or_else(|| Error::UnknownFunction {
                name: function_name.to_string(),
            })
    }

    /// Get a metric by name
    pub fn get_metric<'a>(&'a self, metric_name: &str) -> Result<&'a MetricConfig, Error> {
        self.metrics
            .get(metric_name)
            .ok_or_else(|| Error::UnknownMetric {
                name: metric_name.to_string(),
            })
    }

    /// Get a tool by name
    pub fn get_tool<'a>(&'a self, tool_name: &str) -> Result<&'a StaticToolConfig, Error> {
        self.tools.get(tool_name).ok_or_else(|| Error::UnknownTool {
            name: tool_name.to_string(),
        })
    }

    /// Get a model by name
    pub fn get_model<'a>(&'a self, model_name: &str) -> Result<&'a ModelConfig, Error> {
        self.models
            .get(model_name)
            .ok_or_else(|| Error::UnknownModel {
                name: model_name.to_string(),
            })
    }

    /// Get all templates from the config
    /// The HashMap returned is a mapping from the path as given in the TOML file
    /// (relative to the directory containing the TOML file) to the path on the filesystem.
    /// The former path is used as the name of the template for retrival by variants later.
    pub fn get_templates<P: AsRef<Path>>(&self, base_path: P) -> HashMap<String, PathBuf> {
        let mut templates = HashMap::new();
        let mut add_template = |path: &Option<PathBuf>| {
            if let Some(ref path) = path {
                templates.insert(
                    // This `to_string_lossy`is there to handle OSes where paths
                    // cannot be represented in UTF-8.
                    path.to_string_lossy().to_string(),
                    base_path.as_ref().join(path),
                );
            }
        };

        for function in self.functions.values() {
            for variant in function.variants().values() {
                match variant {
                    VariantConfig::ChatCompletion(chat_config) => {
                        add_template(&chat_config.system_template);
                        add_template(&chat_config.user_template);
                        add_template(&chat_config.assistant_template);
                    }
                }
            }
        }
        templates
    }
}

/// This struct is used to deserialize the TOML config file
/// It does not contain the information that needs to be loaded from the filesystem
/// such as the JSON schemas for the functions and tools.
/// If should be used as part of the `Config::load` method only.
///
/// This allows us to avoid using Option types to represent variables that are initialized after the
/// config is initially parsed.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UninitializedConfig {
    pub gateway: Option<GatewayConfig>,
    pub models: HashMap<String, ModelConfig>, // model name => model config
    pub functions: HashMap<String, UninitializedFunctionConfig>, // function name => function config
    #[serde(default)]
    pub metrics: HashMap<String, MetricConfig>, // metric name => metric config
    #[serde(default)]
    pub tools: HashMap<String, UninitializedToolConfig>, // tool name => tool config
}

impl UninitializedConfig {
    /// Load and validate the TensorZero config file

    /// Use a path provided as a CLI argument (`./gateway path/to/tensorzero.toml`), or default to
    /// `tensorzero.toml` in the current directory if no path is provided.
    fn get_config_path() -> String {
        match std::env::args().nth(1) {
            Some(path) => path,
            None => "config/tensorzero.toml".to_string(),
        }
    }

    /// Read a file from the file system and parse it as TOML
    fn read_toml_config(path: &str) -> Result<toml::Table, Error> {
        std::fs::read_to_string(path)
            .map_err(|_| Error::Config {
                message: format!("Failed to read config file: {path}"),
            })?
            .parse::<toml::Table>()
            .map_err(|_| Error::Config {
                message: format!("Failed to parse config file as valid TOML: {path}"),
            })
    }
}

/// Deserialize a TOML table into `UninitializedConfig`
impl TryFrom<toml::Table> for UninitializedConfig {
    type Error = Error;

    fn try_from(table: toml::Table) -> Result<Self, Self::Error> {
        // NOTE: We'd like to use `serde_path_to_error` here but it has a bug with enums:
        //       https://github.com/dtolnay/path-to-error/issues/1
        match table.try_into() {
            Ok(config) => Ok(config),
            Err(e) => Err(Error::Config {
                message: format!("Failed to parse config:\n{e}"),
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "lowercase")]
#[serde(deny_unknown_fields)]
enum UninitializedFunctionConfig {
    Chat(UninitializedFunctionConfigChat),
    Json(UninitializedFunctionConfigJson),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UninitializedFunctionConfigChat {
    variants: HashMap<String, VariantConfig>, // variant name => variant config
    system_schema: Option<PathBuf>,
    user_schema: Option<PathBuf>,
    assistant_schema: Option<PathBuf>,
    #[serde(default)]
    tools: Vec<String>, // tool names
    #[serde(default)]
    tool_choice: ToolChoice,
    #[serde(default)]
    parallel_tool_calls: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UninitializedFunctionConfigJson {
    variants: HashMap<String, VariantConfig>, // variant name => variant config
    system_schema: Option<PathBuf>,
    user_schema: Option<PathBuf>,
    assistant_schema: Option<PathBuf>,
    output_schema: PathBuf, // schema is mandatory for JSON functions
}

impl UninitializedFunctionConfig {
    pub fn load<P: AsRef<Path>>(self, base_path: P) -> Result<FunctionConfig, Error> {
        match self {
            UninitializedFunctionConfig::Chat(params) => {
                let system_schema = params
                    .system_schema
                    .map(|path| JSONSchemaFromPath::new(path, base_path.as_ref()))
                    .transpose()?;
                let user_schema = params
                    .user_schema
                    .map(|path| JSONSchemaFromPath::new(path, base_path.as_ref()))
                    .transpose()?;
                let assistant_schema = params
                    .assistant_schema
                    .map(|path| JSONSchemaFromPath::new(path, base_path.as_ref()))
                    .transpose()?;
                Ok(FunctionConfig::Chat(FunctionConfigChat {
                    variants: params.variants,
                    system_schema,
                    user_schema,
                    assistant_schema,
                    tools: params.tools,
                    tool_choice: params.tool_choice,
                    parallel_tool_calls: params.parallel_tool_calls,
                }))
            }
            UninitializedFunctionConfig::Json(params) => {
                let system_schema = params
                    .system_schema
                    .map(|path| JSONSchemaFromPath::new(path, base_path.as_ref()))
                    .transpose()?;
                let user_schema = params
                    .user_schema
                    .map(|path| JSONSchemaFromPath::new(path, base_path.as_ref()))
                    .transpose()?;
                let assistant_schema = params
                    .assistant_schema
                    .map(|path| JSONSchemaFromPath::new(path, base_path.as_ref()))
                    .transpose()?;
                let output_schema =
                    JSONSchemaFromPath::new(params.output_schema.clone(), base_path.as_ref())?;
                let implicit_tool_output_schema =
                    JSONSchemaFromPath::new(params.output_schema, base_path.as_ref())?;
                let implicit_tool = ToolConfig::Implicit(ImplicitToolConfig {
                    parameters: implicit_tool_output_schema,
                });
                let implicit_tool_call_config = ToolCallConfig {
                    tools_available: vec![implicit_tool],
                    tool_choice: ToolChoice::Specific(IMPLICIT_TOOL_NAME.to_string()),
                    parallel_tool_calls: false,
                };
                Ok(FunctionConfig::Json(FunctionConfigJson {
                    variants: params.variants,
                    system_schema,
                    user_schema,
                    assistant_schema,
                    output_schema,
                    implicit_tool_call_config,
                }))
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UninitializedToolConfig {
    pub description: String,
    pub parameters: PathBuf,
    #[serde(default)]
    pub strict: bool,
}

impl UninitializedToolConfig {
    pub fn load<P: AsRef<Path>>(
        self,
        base_path: P,
        name: String,
    ) -> Result<StaticToolConfig, Error> {
        let parameters = JSONSchemaFromPath::new(self.parameters, base_path.as_ref())?;
        Ok(StaticToolConfig {
            name,
            description: self.description,
            parameters,
            strict: self.strict,
        })
    }
}

#[cfg(test)]
mod tests {

    use crate::variant::JsonMode;

    use super::*;

    /// Ensure that the sample valid config can be parsed without panicking
    #[test]
    fn test_config_from_toml_table_valid() {
        let config = get_sample_valid_config();
        let base_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        Config::load_from_toml(config, base_path.clone()).expect("Failed to load config");

        // Ensure that removing the `[metrics]` section still parses the config
        let mut config = get_sample_valid_config();
        config
            .remove("metrics")
            .expect("Failed to remove `[metrics]` section");
        let config = Config::load_from_toml(config, base_path).expect("Failed to load config");

        // Check that the JSON mode is set properly on the JSON variants
        let prompt_a_json_mode = match config
            .functions
            .get("json_with_schemas")
            .unwrap()
            .variants()
            .get("openai_promptA")
            .unwrap()
        {
            VariantConfig::ChatCompletion(chat_config) => &chat_config.json_mode,
        };
        assert_eq!(prompt_a_json_mode, &JsonMode::ImplicitTool);

        let prompt_b_json_mode = match config
            .functions
            .get("json_with_schemas")
            .unwrap()
            .variants()
            .get("openai_promptB")
            .unwrap()
        {
            VariantConfig::ChatCompletion(chat_config) => &chat_config.json_mode,
        };
        assert_eq!(prompt_b_json_mode, &JsonMode::On);
        // Check that the tool choice for get_weather is set to "specific" and the correct tool
        let function = config.functions.get("weather_helper").unwrap();
        match function {
            FunctionConfig::Chat(chat_config) => {
                assert_eq!(
                    chat_config.tool_choice,
                    ToolChoice::Specific("get_temperature".to_string())
                );
            }
            _ => panic!("Expected a chat function"),
        }
    }

    /// Ensure that the config parsing correctly handles the `gateway.bind_address` field
    #[test]
    fn test_config_gateway_bind_address() {
        let mut config = get_sample_valid_config();
        let base_path = PathBuf::new();

        // Test with a valid bind address
        let parsed_config = Config::load_from_toml(config.clone(), base_path.clone()).unwrap();
        assert_eq!(
            parsed_config.gateway.bind_address.unwrap().to_string(),
            "0.0.0.0:3000"
        );

        // Test with missing gateway section
        config.remove("gateway");
        let parsed_config = Config::load_from_toml(config.clone(), base_path.clone()).unwrap();
        assert!(parsed_config.gateway.bind_address.is_none());

        // Test with missing bind_address
        config.insert(
            "gateway".to_string(),
            toml::Value::Table(toml::Table::new()),
        );
        let parsed_config = Config::load_from_toml(config.clone(), base_path.clone()).unwrap();
        assert!(parsed_config.gateway.bind_address.is_none());

        // Test with invalid bind address
        config["gateway"].as_table_mut().unwrap().insert(
            "bind_address".to_string(),
            toml::Value::String("invalid_address".to_string()),
        );
        let result = Config::load_from_toml(config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::Config {
                message: "Failed to parse config:\ninvalid socket address syntax\nin `gateway.bind_address`\n".to_string()
            }
        );
    }

    /// Ensure that the config parsing fails when the `[models]` section is missing
    #[test]
    fn test_config_from_toml_table_missing_models() {
        let mut config = get_sample_valid_config();
        let base_path = PathBuf::new();
        config
            .remove("models")
            .expect("Failed to remove `[models]` section");

        assert_eq!(
            Config::load_from_toml(config, base_path).unwrap_err(),
            Error::Config {
                message: "Failed to parse config:\nmissing field `models`\n".to_string()
            }
        );
    }

    /// Ensure that the config parsing fails when the `[providers]` section is missing
    #[test]
    fn test_config_from_toml_table_missing_providers() {
        let mut config = get_sample_valid_config();
        config["models"]["claude-3-haiku-20240307"]
            .as_table_mut()
            .expect("Failed to get `models.claude-3-haiku-20240307` section")
            .remove("providers")
            .expect("Failed to remove `[providers]` section");
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::Config {
                message: "Failed to parse config:\nmissing field `providers`\nin `models.claude-3-haiku-20240307`\n".to_string()
            }
        );
    }

    /// Ensure that the config parsing fails when the `[functions]` section is missing
    #[test]
    fn test_config_from_toml_table_missing_functions() {
        let mut config = get_sample_valid_config();
        config
            .remove("functions")
            .expect("Failed to remove `[functions]` section");
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::Config {
                message: "Failed to parse config:\nmissing field `functions`\n".to_string()
            }
        );
    }

    /// Ensure that the config parsing fails when the `[variants]` section is missing
    #[test]
    fn test_config_from_toml_table_missing_variants() {
        let mut config = get_sample_valid_config();
        config["functions"]["generate_draft"]
            .as_table_mut()
            .expect("Failed to get `functions.generate_draft` section")
            .remove("variants")
            .expect("Failed to remove `[variants]` section");
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::Config {
                message:
                    "Failed to parse config:\nmissing field `variants`\nin `functions.generate_draft`\n"
                        .to_string()
            }
        );
    }

    /// Ensure that the config parsing fails when there are extra variables at the root level
    #[test]
    fn test_config_from_toml_table_extra_variables_root() {
        let mut config = get_sample_valid_config();
        config.insert("enable_agi".into(), true.into());
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(config, base_path);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to parse config:\nunknown field `enable_agi`, expected one of"));
    }

    /// Ensure that the config parsing fails when there are extra variables for models
    #[test]
    fn test_config_from_toml_table_extra_variables_models() {
        let mut config = get_sample_valid_config();
        config["models"]["claude-3-haiku-20240307"]
            .as_table_mut()
            .expect("Failed to get `models.claude-3-haiku-20240307` section")
            .insert("enable_agi".into(), true.into());
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(config, base_path);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to parse config:\nunknown field `enable_agi`, expected"));
    }

    /// Ensure that the config parsing fails when there are extra variables for providers
    #[test]
    fn test_config_from_toml_table_extra_variables_providers() {
        let mut config = get_sample_valid_config();
        config["models"]["claude-3-haiku-20240307"]["providers"]["anthropic"]
            .as_table_mut()
            .expect("Failed to get `models.claude-3-haiku-20240307.providers.anthropic` section")
            .insert("enable_agi".into(), true.into());
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(config, base_path);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to parse config:\nunknown field `enable_agi`, expected"));
    }

    /// Ensure that the config parsing fails when there are extra variables for functions
    #[test]
    fn test_config_from_toml_table_extra_variables_functions() {
        let mut config = get_sample_valid_config();
        config["functions"]["generate_draft"]
            .as_table_mut()
            .expect("Failed to get `functions.generate_draft` section")
            .insert("enable_agi".into(), true.into());
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(config, base_path);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to parse config:\nunknown field `enable_agi`, expected"));
    }

    /// Ensure that the config parsing fails when a JSON function has no output schema
    #[test]
    fn test_config_from_toml_table_json_function_no_output_schema() {
        let mut config = get_sample_valid_config();
        config["functions"]["json_with_schemas"]
            .as_table_mut()
            .expect("Failed to get `functions.generate_draft` section")
            .remove("output_schema");
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(config, base_path);
        assert!(result.unwrap_err().to_string().contains(
            "Failed to parse config:\nmissing field `output_schema`\nin `functions.json_with_schemas`\n"
        ));
    }

    /// Ensure that the config parsing fails when there are extra variables for variants
    #[test]
    fn test_config_from_toml_table_extra_variables_variants() {
        let mut config = get_sample_valid_config();
        config["functions"]["generate_draft"]["variants"]["openai_promptA"]
            .as_table_mut()
            .expect("Failed to get `functions.generate_draft.variants.openai_promptA` section")
            .insert("enable_agi".into(), true.into());
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(config, base_path);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to parse config:\nunknown field `enable_agi`, expected"));
    }

    /// Ensure that the config parsing fails when there are extra variables for metrics
    #[test]
    fn test_config_from_toml_table_extra_variables_metrics() {
        let mut config = get_sample_valid_config();
        config["metrics"]["task_success"]
            .as_table_mut()
            .expect("Failed to get `metrics.task_success` section")
            .insert("enable_agi".into(), true.into());
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(config, base_path);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to parse config:\nunknown field `enable_agi`, expected"));
    }

    /// Ensure that the config validation fails when a model has no providers in `routing`
    #[test]
    fn test_config_validate_model_empty_providers() {
        let mut config = get_sample_valid_config();
        config["models"]["gpt-3.5-turbo"]["routing"] = toml::Value::Array(vec![]);
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::Config {
                message: "Invalid Config: `models.gpt-3.5-turbo`: `routing` must not be empty"
                    .to_string()
            }
        );
    }

    /// Ensure that the config validation fails when there are duplicate routing entries
    #[test]
    fn test_config_validate_model_duplicate_routing_entry() {
        let mut config = get_sample_valid_config();
        config["models"]["gpt-3.5-turbo"]["routing"] =
            toml::Value::Array(vec!["openai".into(), "openai".into()]);
        let result = Config::load_from_toml(config, PathBuf::new());
        assert_eq!(
            result.unwrap_err(),
            Error::Config {
                message: "Invalid Config: `models.gpt-3.5-turbo.routing`: duplicate entry `openai`"
                    .to_string()
            }
        );
    }

    /// Ensure that the config validation fails when a routing entry does not exist in providers
    #[test]
    fn test_config_validate_model_routing_entry_not_in_providers() {
        let mut config = get_sample_valid_config();
        config["models"]["gpt-3.5-turbo"]["routing"] = toml::Value::Array(vec!["closedai".into()]);
        let result = Config::load_from_toml(config, PathBuf::new());
        assert_eq!(
            result.unwrap_err(),
            Error::Config {
                message: "Invalid Config: `models.gpt-3.5-turbo`: `routing` contains entry `closedai` that does not exist in `providers`"
                    .to_string()
            }
        );
    }

    /// Ensure that the config loading fails when the system schema does not exist
    #[test]
    fn test_config_system_schema_does_not_exist() {
        let mut sample_config = get_sample_valid_config();
        sample_config["functions"]["templates_with_variables_chat"]["system_schema"] =
            "non_existent_file.json".into();
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(sample_config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::JsonSchema {
                message: "Failed to read JSON Schema `non_existent_file.json`: No such file or directory (os error 2)".to_string()
            }
        );
        let mut sample_config = get_sample_valid_config();
        sample_config["functions"]["templates_with_variables_json"]["system_schema"] =
            "non_existent_file.json".into();
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(sample_config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::JsonSchema {
                message: "Failed to read JSON Schema `non_existent_file.json`: No such file or directory (os error 2)".to_string()
            }
        );
    }

    /// Ensure that the config loading fails when the user schema does not exist
    #[test]
    fn test_config_user_schema_does_not_exist() {
        let mut sample_config = get_sample_valid_config();
        sample_config["functions"]["templates_with_variables_chat"]["user_schema"] =
            "non_existent_file.json".into();
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(sample_config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::JsonSchema {
                message: "Failed to read JSON Schema `non_existent_file.json`: No such file or directory (os error 2)".to_string()
            }
        );
        let mut sample_config = get_sample_valid_config();
        sample_config["functions"]["templates_with_variables_json"]["user_schema"] =
            "non_existent_file.json".into();
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(sample_config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::JsonSchema {
                message: "Failed to read JSON Schema `non_existent_file.json`: No such file or directory (os error 2)".to_string()
            }
        );
    }

    /// Ensure that the config loading fails when the assistant schema does not exist
    #[test]
    fn test_config_assistant_schema_does_not_exist() {
        let mut sample_config = get_sample_valid_config();
        sample_config["functions"]["templates_with_variables_chat"]["assistant_schema"] =
            "non_existent_file.json".into();
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(sample_config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::JsonSchema {
                message: "Failed to read JSON Schema `non_existent_file.json`: No such file or directory (os error 2)".to_string()
            }
        );
        let mut sample_config = get_sample_valid_config();
        sample_config["functions"]["templates_with_variables_json"]["assistant_schema"] =
            "non_existent_file.json".into();
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(sample_config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::JsonSchema {
                message: "Failed to read JSON Schema `non_existent_file.json`: No such file or directory (os error 2)".to_string()
            }
        );
    }

    /// Ensure that the config loading fails when the system schema is missing but is needed
    #[test]
    fn test_config_system_schema_is_needed() {
        let mut sample_config = get_sample_valid_config();
        sample_config["functions"]["templates_with_variables_chat"]
            .as_table_mut()
            .unwrap()
            .remove("system_schema");
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(sample_config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::Config {
                message: "Invalid Config: `functions.templates_with_variables_chat.variants.variant_with_variables`: `system_schema` is required when `system_template` is specified".to_string()
            }
        );
        let mut sample_config = get_sample_valid_config();
        sample_config["functions"]["templates_with_variables_json"]
            .as_table_mut()
            .unwrap()
            .remove("system_schema");
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(sample_config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::Config {
                message: "Invalid Config: `functions.templates_with_variables_json.variants.variant_with_variables`: `system_schema` is required when `system_template` is specified".to_string()
            }
        );
    }

    /// Ensure that the config loading fails when the user schema is missing but is needed
    #[test]
    fn test_config_user_schema_is_needed() {
        let mut sample_config = get_sample_valid_config();
        sample_config["functions"]["templates_with_variables_chat"]
            .as_table_mut()
            .unwrap()
            .remove("user_schema");
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(sample_config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::Config {
                message: "Invalid Config: `functions.templates_with_variables_chat.variants.variant_with_variables`: `user_schema` is required when `user_template` is specified".to_string()
            }
        );

        let mut sample_config = get_sample_valid_config();
        sample_config["functions"]["templates_with_variables_json"]
            .as_table_mut()
            .unwrap()
            .remove("user_schema");
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(sample_config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::Config {
                message: "Invalid Config: `functions.templates_with_variables_json.variants.variant_with_variables`: `user_schema` is required when `user_template` is specified".to_string()
            }
        );
    }

    /// Ensure that the config loading fails when the assistant schema is missing but is needed
    #[test]
    fn test_config_assistant_schema_is_needed() {
        let mut sample_config = get_sample_valid_config();
        sample_config["functions"]["templates_with_variables_chat"]
            .as_table_mut()
            .unwrap()
            .remove("assistant_schema");
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(sample_config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::Config {
                message: "Invalid Config: `functions.templates_with_variables_chat.variants.variant_with_variables`: `assistant_schema` is required when `assistant_template` is specified".to_string()
            }
        );
        let mut sample_config = get_sample_valid_config();
        sample_config["functions"]["templates_with_variables_json"]
            .as_table_mut()
            .unwrap()
            .remove("assistant_schema");
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(sample_config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::Config {
                message: "Invalid Config: `functions.templates_with_variables_json.variants.variant_with_variables`: `assistant_schema` is required when `assistant_template` is specified".to_string()
            }
        );
    }

    /// Ensure that the config validation fails when a function variant has a negative weight
    #[test]
    fn test_config_validate_function_variant_negative_weight() {
        let mut config = get_sample_valid_config();
        config["functions"]["generate_draft"]["variants"]["openai_promptA"]["weight"] =
            toml::Value::Float(-1.0);
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(config, base_path);
        assert_eq!(
            result.unwrap_err(),
            Error::Config {
                message: "Invalid Config: `functions.generate_draft.variants.openai_promptA.weight`: must be non-negative".to_string()
            }
        );
    }

    /// Ensure that the config validation fails when a variant has a model that does not exist in the models section
    #[test]
    fn test_config_validate_variant_model_not_in_models() {
        let mut config = get_sample_valid_config();
        config["functions"]["generate_draft"]["variants"]["openai_promptA"]["model"] =
            "non_existent_model".into();
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(config, base_path);

        assert_eq!(
            result.unwrap_err(),
            Error::Config {
                message: "Invalid Config: `functions.generate_draft.variants.openai_promptA`: `model` must be a valid model name".to_string()
            }
        );
    }

    /// Ensure that the config validation fails when a variant has a model that does not exist in the models section
    #[test]
    fn test_config_validate_variant_nonexistent_tool() {
        let mut config = get_sample_valid_config();
        config["functions"]["generate_draft"]
            .as_table_mut()
            .unwrap()
            .insert("tools".to_string(), toml::Value::Array(vec![]));
        config["functions"]["generate_draft"]["tools"] =
            toml::Value::Array(vec!["non_existent_tool".into()]);
        let base_path = PathBuf::new();
        let result = Config::load_from_toml(config, base_path);

        assert_eq!(
            result.unwrap_err(),
            Error::Config {
                message: "Invalid Config: `functions.generate_draft.tools`: tool `non_existent_tool` is not present in the config".to_string()
            }
        );
    }

    /// Ensure that get_templates returns the correct templates
    #[test]
    fn test_get_all_templates() {
        let config_table = get_sample_valid_config();
        let config =
            Config::load_from_toml(config_table, PathBuf::new()).expect("Failed to load config");

        // Get all templates
        let templates = config.get_templates(PathBuf::from("/base/path"));

        // Check if all expected templates are present
        assert_eq!(
            templates.get("fixtures/config/functions/generate_draft/promptA/system_template.minijinja"),
            Some(&PathBuf::from(
                "/base/path/fixtures/config/functions/generate_draft/promptA/system_template.minijinja"
            ))
        );
        assert_eq!(
            templates.get("fixtures/config/functions/generate_draft/promptA/system_template.minijinja"),
            Some(&PathBuf::from(
                "/base/path/fixtures/config/functions/generate_draft/promptA/system_template.minijinja"
            ))
        );
        assert_eq!(
            templates.get("fixtures/config/functions/json_with_schemas/promptA/system_template.minijinja"),
            Some(&PathBuf::from(
                "/base/path/fixtures/config/functions/json_with_schemas/promptA/system_template.minijinja"
            ))
        );
        assert_eq!(
            templates.get("fixtures/config/functions/json_with_schemas/promptB/system_template.minijinja"),
            Some(&PathBuf::from(
                "/base/path/fixtures/config/functions/json_with_schemas/promptB/system_template.minijinja"
            ))
        );
        assert_eq!(
            templates.get("fixtures/config/functions/templates_without_variables/variant_without_templates/system_template.minijinja"),
            Some(&PathBuf::from(
                "/base/path/fixtures/config/functions/templates_without_variables/variant_without_templates/system_template.minijinja"
            ))
        );
        assert_eq!(
            templates.get("fixtures/config/functions/templates_without_variables/variant_without_templates/user_template.minijinja"),
            Some(&PathBuf::from(
                "/base/path/fixtures/config/functions/templates_without_variables/variant_without_templates/user_template.minijinja"
            ))
        );
        assert_eq!(
            templates.get("fixtures/config/functions/templates_without_variables/variant_without_templates/assistant_template.minijinja"),
            Some(&PathBuf::from(
                "/base/path/fixtures/config/functions/templates_without_variables/variant_without_templates/assistant_template.minijinja"
            ))
        );
        assert_eq!(
            templates.get("fixtures/config/functions/templates_with_variables/variant_with_variables/assistant_template.minijinja"),
            Some(&PathBuf::from(
                "/base/path/fixtures/config/functions/templates_with_variables/variant_with_variables/assistant_template.minijinja"
            ))
        );
        assert_eq!(
            templates.get("fixtures/config/functions/templates_with_variables/variant_with_variables/user_template.minijinja"),
            Some(&PathBuf::from(
                "/base/path/fixtures/config/functions/templates_with_variables/variant_with_variables/user_template.minijinja"
            ))
        );
        assert_eq!(
            templates.get("fixtures/config/functions/templates_with_variables/variant_with_variables/system_template.minijinja"),
            Some(&PathBuf::from(
                "/base/path/fixtures/config/functions/templates_with_variables/variant_with_variables/system_template.minijinja"
            ))
        );

        // Check the total number of templates
        assert_eq!(templates.len(), 10);
    }

    /// Get a sample valid config for testing
    fn get_sample_valid_config() -> toml::Table {
        let config_str = r#"
        # ┌────────────────────────────────────────────────────────────────────────────┐
        # │                                  GENERAL                                   │
        # └────────────────────────────────────────────────────────────────────────────┘

        [gateway]
        bind_address = "0.0.0.0:3000"

        # ┌────────────────────────────────────────────────────────────────────────────┐
        # │                                   MODELS                                   │
        # └────────────────────────────────────────────────────────────────────────────┘

        [models."gpt-3.5-turbo"]
        routing = ["openai", "azure"]

        [models."gpt-3.5-turbo".providers.openai]
        type = "openai"
        model_name = "gpt-3.5-turbo"

        [models."gpt-3.5-turbo".providers.azure]
        type = "azure"
        deployment_id = "gpt-35-turbo"
        endpoint = "https://your-endpoint.openai.azure.com"

        [models.claude-3-haiku-20240307]
        routing = ["anthropic"]

        [models.claude-3-haiku-20240307.providers.anthropic]
        type = "anthropic"
        model_name = "claude-3-haiku-20240307"

        # ┌────────────────────────────────────────────────────────────────────────────┐
        # │                                 FUNCTIONS                                  │
        # └────────────────────────────────────────────────────────────────────────────┘

        [functions.generate_draft]
        type = "chat"
        system_schema = "fixtures/config/functions/generate_draft/system_schema.json"

        [functions.generate_draft.variants.openai_promptA]
        type = "chat_completion"
        weight = 0.9
        model = "gpt-3.5-turbo"
        system_template = "fixtures/config/functions/generate_draft/promptA/system_template.minijinja"

        [functions.generate_draft.variants.openai_promptB]
        type = "chat_completion"
        weight = 0.1
        model = "gpt-3.5-turbo"
        system_template = "fixtures/config/functions/generate_draft/promptB/system_template.minijinja"

        [functions.json_with_schemas]
        type = "json"
        system_schema = "fixtures/config/functions/json_with_schemas/system_schema.json"
        output_schema = "fixtures/config/functions/json_with_schemas/output_schema.json"

        [functions.json_with_schemas.variants.openai_promptA]
        type = "chat_completion"
        weight = 0.9
        model = "gpt-3.5-turbo"
        system_template = "fixtures/config/functions/json_with_schemas/promptA/system_template.minijinja"
        json_mode = "implicit_tool"

        [functions.json_with_schemas.variants.openai_promptB]
        type = "chat_completion"
        weight = 0.1
        model = "gpt-3.5-turbo"
        system_template = "fixtures/config/functions/json_with_schemas/promptB/system_template.minijinja"

        [functions.weather_helper]
        type = "chat"
        tools = ["get_temperature"]
        tool_choice = {specific = "get_temperature"}

        [functions.weather_helper.variants.openai_promptA]
        type = "chat_completion"
        weight = 1.0
        model = "gpt-3.5-turbo"

        [functions.templates_without_variables_chat]
        type = "chat"

        [functions.templates_without_variables_chat.variants.variant_without_templates]
        type = "chat_completion"
        weight = 1.0
        model = "gpt-3.5-turbo"
        system_template = "fixtures/config/functions/templates_without_variables/variant_without_templates/system_template.minijinja"
        user_template = "fixtures/config/functions/templates_without_variables/variant_without_templates/user_template.minijinja"
        assistant_template = "fixtures/config/functions/templates_without_variables/variant_without_templates/assistant_template.minijinja"

        [functions.templates_with_variables_chat]
        type = "chat"
        system_schema = "fixtures/config/functions/templates_with_variables/system_schema.json"
        user_schema = "fixtures/config/functions/templates_with_variables/user_schema.json"
        assistant_schema = "fixtures/config/functions/templates_with_variables/assistant_schema.json"

        [functions.templates_with_variables_chat.variants.variant_with_variables]
        type = "chat_completion"
        weight = 1.0
        model = "gpt-3.5-turbo"
        system_template = "fixtures/config/functions/templates_with_variables/variant_with_variables/system_template.minijinja"
        user_template = "fixtures/config/functions/templates_with_variables/variant_with_variables/user_template.minijinja"
        assistant_template = "fixtures/config/functions/templates_with_variables/variant_with_variables/assistant_template.minijinja"

        [functions.templates_without_variables_json]
        type = "json"
        output_schema = "fixtures/config/functions/json_with_schemas/output_schema.json"

        [functions.templates_without_variables_json.variants.variant_without_templates]
        type = "chat_completion"
        weight = 1.0
        model = "gpt-3.5-turbo"
        system_template = "fixtures/config/functions/templates_without_variables/variant_without_templates/system_template.minijinja"
        user_template = "fixtures/config/functions/templates_without_variables/variant_without_templates/user_template.minijinja"
        assistant_template = "fixtures/config/functions/templates_without_variables/variant_without_templates/assistant_template.minijinja"

        [functions.templates_with_variables_json]
        type = "json"
        system_schema = "fixtures/config/functions/templates_with_variables/system_schema.json"
        user_schema = "fixtures/config/functions/templates_with_variables/user_schema.json"
        assistant_schema = "fixtures/config/functions/templates_with_variables/assistant_schema.json"
        output_schema = "fixtures/config/functions/json_with_schemas/output_schema.json"

        [functions.templates_with_variables_json.variants.variant_with_variables]
        type = "chat_completion"
        weight = 1.0
        model = "gpt-3.5-turbo"
        system_template = "fixtures/config/functions/templates_with_variables/variant_with_variables/system_template.minijinja"
        user_template = "fixtures/config/functions/templates_with_variables/variant_with_variables/user_template.minijinja"
        assistant_template = "fixtures/config/functions/templates_with_variables/variant_with_variables/assistant_template.minijinja"

        # ┌────────────────────────────────────────────────────────────────────────────┐
        # │                                  METRICS                                   │
        # └────────────────────────────────────────────────────────────────────────────┘

        [metrics.task_success]
        type = "boolean"
        optimize = "max"
        level = "inference"

        [metrics.user_rating]
        type = "float"
        optimize = "max"
        level = "episode"

        # ┌────────────────────────────────────────────────────────────────────────────┐
        # │                                   TOOLS                                    │
        # └────────────────────────────────────────────────────────────────────────────┘
        [tools.get_temperature]
        description = "Get the weather for a given location"
        parameters = "fixtures/config/tools/get_temperature.json"
        "#;

        toml::from_str(config_str).expect("Failed to parse sample config")
    }

    #[test]
    fn test_tensorzero_example_file() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let config_path = format!("{}/../config/tensorzero.toml", manifest_dir);
        let config_pathbuf = PathBuf::from(&config_path);
        let base_path = config_pathbuf
            .parent()
            .expect("Failed to get parent directory of config file");
        let config_table = UninitializedConfig::read_toml_config(&config_path)
            .expect("Failed to read tensorzero.example.toml");

        Config::load_from_toml(config_table, base_path.to_path_buf())
            .expect("Failed to load config");
    }
}
