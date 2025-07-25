use anyhow::{Context as _, Result, anyhow};
use collections::BTreeMap;
use credentials_provider::CredentialsProvider;
use editor::{Editor, EditorElement, EditorStyle};
use futures::{FutureExt, Stream, StreamExt, future::BoxFuture, stream::BoxStream};
use gpui::{
    AnyView, App, AsyncApp, Context, Entity, FontStyle, Subscription, Task, TextStyle, WhiteSpace,
};
use http_client::HttpClient;
use language_model::{
    AuthenticateError, LanguageModel, LanguageModelCompletionError, LanguageModelCompletionEvent,
    LanguageModelId, LanguageModelName, LanguageModelProvider, LanguageModelProviderId,
    LanguageModelProviderName, LanguageModelProviderState, LanguageModelRequest,
    LanguageModelToolChoice, LanguageModelToolResultContent, LanguageModelToolUse, MessageContent,
    RateLimiter, Role, StopReason, TokenUsage,
};
use mistral::StreamResponse;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsStore};
use std::collections::HashMap;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use strum::IntoEnumIterator;
use theme::ThemeSettings;
use ui::{Icon, IconName, List, Tooltip, prelude::*};
use util::ResultExt;

use crate::{AllLanguageModelSettings, ui::InstructionListItem};

const PROVIDER_ID: LanguageModelProviderId = LanguageModelProviderId::new("mistral");
const PROVIDER_NAME: LanguageModelProviderName = LanguageModelProviderName::new("Mistral");

#[derive(Default, Clone, Debug, PartialEq)]
pub struct MistralSettings {
    pub api_url: String,
    pub available_models: Vec<AvailableModel>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AvailableModel {
    pub name: String,
    pub display_name: Option<String>,
    pub max_tokens: u64,
    pub max_output_tokens: Option<u64>,
    pub max_completion_tokens: Option<u64>,
    pub supports_tools: Option<bool>,
    pub supports_images: Option<bool>,
}

pub struct MistralLanguageModelProvider {
    http_client: Arc<dyn HttpClient>,
    state: gpui::Entity<State>,
}

pub struct State {
    api_key: Option<String>,
    api_key_from_env: bool,
    _subscription: Subscription,
}

const MISTRAL_API_KEY_VAR: &str = "MISTRAL_API_KEY";

impl State {
    fn is_authenticated(&self) -> bool {
        self.api_key.is_some()
    }

    fn reset_api_key(&self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let credentials_provider = <dyn CredentialsProvider>::global(cx);
        let api_url = AllLanguageModelSettings::get_global(cx)
            .mistral
            .api_url
            .clone();
        cx.spawn(async move |this, cx| {
            credentials_provider
                .delete_credentials(&api_url, &cx)
                .await
                .log_err();
            this.update(cx, |this, cx| {
                this.api_key = None;
                this.api_key_from_env = false;
                cx.notify();
            })
        })
    }

    fn set_api_key(&mut self, api_key: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        let credentials_provider = <dyn CredentialsProvider>::global(cx);
        let api_url = AllLanguageModelSettings::get_global(cx)
            .mistral
            .api_url
            .clone();
        cx.spawn(async move |this, cx| {
            credentials_provider
                .write_credentials(&api_url, "Bearer", api_key.as_bytes(), &cx)
                .await?;
            this.update(cx, |this, cx| {
                this.api_key = Some(api_key);
                cx.notify();
            })
        })
    }

    fn authenticate(&self, cx: &mut Context<Self>) -> Task<Result<(), AuthenticateError>> {
        if self.is_authenticated() {
            return Task::ready(Ok(()));
        }

        let credentials_provider = <dyn CredentialsProvider>::global(cx);
        let api_url = AllLanguageModelSettings::get_global(cx)
            .mistral
            .api_url
            .clone();
        cx.spawn(async move |this, cx| {
            let (api_key, from_env) = if let Ok(api_key) = std::env::var(MISTRAL_API_KEY_VAR) {
                (api_key, true)
            } else {
                let (_, api_key) = credentials_provider
                    .read_credentials(&api_url, &cx)
                    .await?
                    .ok_or(AuthenticateError::CredentialsNotFound)?;
                (
                    String::from_utf8(api_key).context("invalid {PROVIDER_NAME} API key")?,
                    false,
                )
            };
            this.update(cx, |this, cx| {
                this.api_key = Some(api_key);
                this.api_key_from_env = from_env;
                cx.notify();
            })?;

            Ok(())
        })
    }
}

impl MistralLanguageModelProvider {
    pub fn new(http_client: Arc<dyn HttpClient>, cx: &mut App) -> Self {
        let state = cx.new(|cx| State {
            api_key: None,
            api_key_from_env: false,
            _subscription: cx.observe_global::<SettingsStore>(|_this: &mut State, cx| {
                cx.notify();
            }),
        });

        Self { http_client, state }
    }

    fn create_language_model(&self, model: mistral::Model) -> Arc<dyn LanguageModel> {
        Arc::new(MistralLanguageModel {
            id: LanguageModelId::from(model.id().to_string()),
            model,
            state: self.state.clone(),
            http_client: self.http_client.clone(),
            request_limiter: RateLimiter::new(4),
        })
    }
}

impl LanguageModelProviderState for MistralLanguageModelProvider {
    type ObservableEntity = State;

    fn observable_entity(&self) -> Option<gpui::Entity<Self::ObservableEntity>> {
        Some(self.state.clone())
    }
}

impl LanguageModelProvider for MistralLanguageModelProvider {
    fn id(&self) -> LanguageModelProviderId {
        PROVIDER_ID
    }

    fn name(&self) -> LanguageModelProviderName {
        PROVIDER_NAME
    }

    fn icon(&self) -> IconName {
        IconName::AiMistral
    }

    fn default_model(&self, _cx: &App) -> Option<Arc<dyn LanguageModel>> {
        Some(self.create_language_model(mistral::Model::default()))
    }

    fn default_fast_model(&self, _cx: &App) -> Option<Arc<dyn LanguageModel>> {
        Some(self.create_language_model(mistral::Model::default_fast()))
    }

    fn provided_models(&self, cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        let mut models = BTreeMap::default();

        // Add base models from mistral::Model::iter()
        for model in mistral::Model::iter() {
            if !matches!(model, mistral::Model::Custom { .. }) {
                models.insert(model.id().to_string(), model);
            }
        }

        // Override with available models from settings
        for model in &AllLanguageModelSettings::get_global(cx)
            .mistral
            .available_models
        {
            models.insert(
                model.name.clone(),
                mistral::Model::Custom {
                    name: model.name.clone(),
                    display_name: model.display_name.clone(),
                    max_tokens: model.max_tokens,
                    max_output_tokens: model.max_output_tokens,
                    max_completion_tokens: model.max_completion_tokens,
                    supports_tools: model.supports_tools,
                    supports_images: model.supports_images,
                },
            );
        }

        models
            .into_values()
            .map(|model| {
                Arc::new(MistralLanguageModel {
                    id: LanguageModelId::from(model.id().to_string()),
                    model,
                    state: self.state.clone(),
                    http_client: self.http_client.clone(),
                    request_limiter: RateLimiter::new(4),
                }) as Arc<dyn LanguageModel>
            })
            .collect()
    }

    fn is_authenticated(&self, cx: &App) -> bool {
        self.state.read(cx).is_authenticated()
    }

    fn authenticate(&self, cx: &mut App) -> Task<Result<(), AuthenticateError>> {
        self.state.update(cx, |state, cx| state.authenticate(cx))
    }

    fn configuration_view(&self, window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| ConfigurationView::new(self.state.clone(), window, cx))
            .into()
    }

    fn reset_credentials(&self, cx: &mut App) -> Task<Result<()>> {
        self.state.update(cx, |state, cx| state.reset_api_key(cx))
    }
}

pub struct MistralLanguageModel {
    id: LanguageModelId,
    model: mistral::Model,
    state: gpui::Entity<State>,
    http_client: Arc<dyn HttpClient>,
    request_limiter: RateLimiter,
}

impl MistralLanguageModel {
    fn stream_completion(
        &self,
        request: mistral::Request,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<futures::stream::BoxStream<'static, Result<mistral::StreamResponse>>>,
    > {
        let http_client = self.http_client.clone();
        let Ok((api_key, api_url)) = cx.read_entity(&self.state, |state, cx| {
            let settings = &AllLanguageModelSettings::get_global(cx).mistral;
            (state.api_key.clone(), settings.api_url.clone())
        }) else {
            return futures::future::ready(Err(anyhow!("App state dropped"))).boxed();
        };

        let future = self.request_limiter.stream(async move {
            let api_key = api_key.context("Missing Mistral API Key")?;
            let request =
                mistral::stream_completion(http_client.as_ref(), &api_url, &api_key, request);
            let response = request.await?;
            Ok(response)
        });

        async move { Ok(future.await?.boxed()) }.boxed()
    }
}

impl LanguageModel for MistralLanguageModel {
    fn id(&self) -> LanguageModelId {
        self.id.clone()
    }

    fn name(&self) -> LanguageModelName {
        LanguageModelName::from(self.model.display_name().to_string())
    }

    fn provider_id(&self) -> LanguageModelProviderId {
        PROVIDER_ID
    }

    fn provider_name(&self) -> LanguageModelProviderName {
        PROVIDER_NAME
    }

    fn supports_tools(&self) -> bool {
        self.model.supports_tools()
    }

    fn supports_tool_choice(&self, _choice: LanguageModelToolChoice) -> bool {
        self.model.supports_tools()
    }

    fn supports_images(&self) -> bool {
        self.model.supports_images()
    }

    fn telemetry_id(&self) -> String {
        format!("mistral/{}", self.model.id())
    }

    fn max_token_count(&self) -> u64 {
        self.model.max_token_count()
    }

    fn max_output_tokens(&self) -> Option<u64> {
        self.model.max_output_tokens()
    }

    fn count_tokens(
        &self,
        request: LanguageModelRequest,
        cx: &App,
    ) -> BoxFuture<'static, Result<u64>> {
        cx.background_spawn(async move {
            let messages = request
                .messages
                .into_iter()
                .map(|message| tiktoken_rs::ChatCompletionRequestMessage {
                    role: match message.role {
                        Role::User => "user".into(),
                        Role::Assistant => "assistant".into(),
                        Role::System => "system".into(),
                    },
                    content: Some(message.string_contents()),
                    name: None,
                    function_call: None,
                })
                .collect::<Vec<_>>();

            tiktoken_rs::num_tokens_from_messages("gpt-4", &messages).map(|tokens| tokens as u64)
        })
        .boxed()
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            BoxStream<'static, Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>,
            LanguageModelCompletionError,
        >,
    > {
        let request = into_mistral(
            request,
            self.model.id().to_string(),
            self.max_output_tokens(),
        );
        let stream = self.stream_completion(request, cx);

        async move {
            let stream = stream.await?;
            let mapper = MistralEventMapper::new();
            Ok(mapper.map_stream(stream).boxed())
        }
        .boxed()
    }
}

pub fn into_mistral(
    request: LanguageModelRequest,
    model: String,
    max_output_tokens: Option<u64>,
) -> mistral::Request {
    let stream = true;

    let mut messages = Vec::new();
    for message in &request.messages {
        match message.role {
            Role::User => {
                let mut message_content = mistral::MessageContent::empty();
                for content in &message.content {
                    match content {
                        MessageContent::Text(text) => {
                            message_content
                                .push_part(mistral::MessagePart::Text { text: text.clone() });
                        }
                        MessageContent::Image(image_content) => {
                            message_content.push_part(mistral::MessagePart::ImageUrl {
                                image_url: image_content.to_base64_url(),
                            });
                        }
                        MessageContent::Thinking { text, .. } => {
                            message_content
                                .push_part(mistral::MessagePart::Text { text: text.clone() });
                        }
                        MessageContent::RedactedThinking(_) => {}
                        MessageContent::ToolUse(_) => {
                            // Tool use is not supported in User messages for Mistral
                        }
                        MessageContent::ToolResult(tool_result) => {
                            let tool_content = match &tool_result.content {
                                LanguageModelToolResultContent::Text(text) => text.to_string(),
                                LanguageModelToolResultContent::Image(_) => {
                                    "[Tool responded with an image, but Zed doesn't support these in Mistral models yet]".to_string()
                                }
                            };
                            messages.push(mistral::RequestMessage::Tool {
                                content: tool_content,
                                tool_call_id: tool_result.tool_use_id.to_string(),
                            });
                        }
                    }
                }
                if !matches!(message_content, mistral::MessageContent::Plain { ref content } if content.is_empty())
                {
                    messages.push(mistral::RequestMessage::User {
                        content: message_content,
                    });
                }
            }
            Role::Assistant => {
                for content in &message.content {
                    match content {
                        MessageContent::Text(text) | MessageContent::Thinking { text, .. } => {
                            messages.push(mistral::RequestMessage::Assistant {
                                content: Some(text.clone()),
                                tool_calls: Vec::new(),
                            });
                        }
                        MessageContent::RedactedThinking(_) => {}
                        MessageContent::Image(_) => {}
                        MessageContent::ToolUse(tool_use) => {
                            let tool_call = mistral::ToolCall {
                                id: tool_use.id.to_string(),
                                content: mistral::ToolCallContent::Function {
                                    function: mistral::FunctionContent {
                                        name: tool_use.name.to_string(),
                                        arguments: serde_json::to_string(&tool_use.input)
                                            .unwrap_or_default(),
                                    },
                                },
                            };

                            if let Some(mistral::RequestMessage::Assistant { tool_calls, .. }) =
                                messages.last_mut()
                            {
                                tool_calls.push(tool_call);
                            } else {
                                messages.push(mistral::RequestMessage::Assistant {
                                    content: None,
                                    tool_calls: vec![tool_call],
                                });
                            }
                        }
                        MessageContent::ToolResult(_) => {
                            // Tool results are not supported in Assistant messages
                        }
                    }
                }
            }
            Role::System => {
                for content in &message.content {
                    match content {
                        MessageContent::Text(text) | MessageContent::Thinking { text, .. } => {
                            messages.push(mistral::RequestMessage::System {
                                content: text.clone(),
                            });
                        }
                        MessageContent::RedactedThinking(_) => {}
                        MessageContent::Image(_)
                        | MessageContent::ToolUse(_)
                        | MessageContent::ToolResult(_) => {
                            // Images and tools are not supported in System messages
                        }
                    }
                }
            }
        }
    }

    // The Mistral API requires that tool messages be followed by assistant messages,
    // not user messages. When we have a tool->user sequence in the conversation,
    // we need to insert a placeholder assistant message to maintain proper conversation
    // flow and prevent API errors. This is a Mistral-specific requirement that differs
    // from other language model APIs.
    let messages = {
        let mut fixed_messages = Vec::with_capacity(messages.len());
        let mut messages_iter = messages.into_iter().peekable();

        while let Some(message) = messages_iter.next() {
            let is_tool_message = matches!(message, mistral::RequestMessage::Tool { .. });
            fixed_messages.push(message);

            // Insert assistant message between tool and user messages
            if is_tool_message {
                if let Some(next_msg) = messages_iter.peek() {
                    if matches!(next_msg, mistral::RequestMessage::User { .. }) {
                        fixed_messages.push(mistral::RequestMessage::Assistant {
                            content: Some(" ".to_string()),
                            tool_calls: Vec::new(),
                        });
                    }
                }
            }
        }

        fixed_messages
    };

    mistral::Request {
        model,
        messages,
        stream,
        max_tokens: max_output_tokens,
        temperature: request.temperature,
        response_format: None,
        tool_choice: match request.tool_choice {
            Some(LanguageModelToolChoice::Auto) if !request.tools.is_empty() => {
                Some(mistral::ToolChoice::Auto)
            }
            Some(LanguageModelToolChoice::Any) if !request.tools.is_empty() => {
                Some(mistral::ToolChoice::Any)
            }
            Some(LanguageModelToolChoice::None) => Some(mistral::ToolChoice::None),
            _ if !request.tools.is_empty() => Some(mistral::ToolChoice::Auto),
            _ => None,
        },
        parallel_tool_calls: if !request.tools.is_empty() {
            Some(false)
        } else {
            None
        },
        tools: request
            .tools
            .into_iter()
            .map(|tool| mistral::ToolDefinition::Function {
                function: mistral::FunctionDefinition {
                    name: tool.name,
                    description: Some(tool.description),
                    parameters: Some(tool.input_schema),
                },
            })
            .collect(),
    }
}

pub struct MistralEventMapper {
    tool_calls_by_index: HashMap<usize, RawToolCall>,
}

impl MistralEventMapper {
    pub fn new() -> Self {
        Self {
            tool_calls_by_index: HashMap::default(),
        }
    }

    pub fn map_stream(
        mut self,
        events: Pin<Box<dyn Send + Stream<Item = Result<StreamResponse>>>>,
    ) -> impl Stream<Item = Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>
    {
        events.flat_map(move |event| {
            futures::stream::iter(match event {
                Ok(event) => self.map_event(event),
                Err(error) => vec![Err(LanguageModelCompletionError::from(error))],
            })
        })
    }

    pub fn map_event(
        &mut self,
        event: mistral::StreamResponse,
    ) -> Vec<Result<LanguageModelCompletionEvent, LanguageModelCompletionError>> {
        let Some(choice) = event.choices.first() else {
            return vec![Err(LanguageModelCompletionError::from(anyhow!(
                "Response contained no choices"
            )))];
        };

        let mut events = Vec::new();
        if let Some(content) = choice.delta.content.clone() {
            events.push(Ok(LanguageModelCompletionEvent::Text(content)));
        }

        if let Some(tool_calls) = choice.delta.tool_calls.as_ref() {
            for tool_call in tool_calls {
                let entry = self.tool_calls_by_index.entry(tool_call.index).or_default();

                if let Some(tool_id) = tool_call.id.clone() {
                    entry.id = tool_id;
                }

                if let Some(function) = tool_call.function.as_ref() {
                    if let Some(name) = function.name.clone() {
                        entry.name = name;
                    }

                    if let Some(arguments) = function.arguments.clone() {
                        entry.arguments.push_str(&arguments);
                    }
                }
            }
        }

        if let Some(usage) = event.usage {
            events.push(Ok(LanguageModelCompletionEvent::UsageUpdate(TokenUsage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            })));
        }

        if let Some(finish_reason) = choice.finish_reason.as_deref() {
            match finish_reason {
                "stop" => {
                    events.push(Ok(LanguageModelCompletionEvent::Stop(StopReason::EndTurn)));
                }
                "tool_calls" => {
                    events.extend(self.process_tool_calls());
                    events.push(Ok(LanguageModelCompletionEvent::Stop(StopReason::ToolUse)));
                }
                unexpected => {
                    log::error!("Unexpected Mistral stop_reason: {unexpected:?}");
                    events.push(Ok(LanguageModelCompletionEvent::Stop(StopReason::EndTurn)));
                }
            }
        }

        events
    }

    fn process_tool_calls(
        &mut self,
    ) -> Vec<Result<LanguageModelCompletionEvent, LanguageModelCompletionError>> {
        let mut results = Vec::new();

        for (_, tool_call) in self.tool_calls_by_index.drain() {
            if tool_call.id.is_empty() || tool_call.name.is_empty() {
                results.push(Err(LanguageModelCompletionError::from(anyhow!(
                    "Received incomplete tool call: missing id or name"
                ))));
                continue;
            }

            match serde_json::Value::from_str(&tool_call.arguments) {
                Ok(input) => results.push(Ok(LanguageModelCompletionEvent::ToolUse(
                    LanguageModelToolUse {
                        id: tool_call.id.into(),
                        name: tool_call.name.into(),
                        is_input_complete: true,
                        input,
                        raw_input: tool_call.arguments,
                    },
                ))),
                Err(error) => {
                    results.push(Ok(LanguageModelCompletionEvent::ToolUseJsonParseError {
                        id: tool_call.id.into(),
                        tool_name: tool_call.name.into(),
                        raw_input: tool_call.arguments.into(),
                        json_parse_error: error.to_string(),
                    }))
                }
            }
        }

        results
    }
}

#[derive(Default)]
struct RawToolCall {
    id: String,
    name: String,
    arguments: String,
}

struct ConfigurationView {
    api_key_editor: Entity<Editor>,
    state: gpui::Entity<State>,
    load_credentials_task: Option<Task<()>>,
}

impl ConfigurationView {
    fn new(state: gpui::Entity<State>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let api_key_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("0aBCDEFGhIjKLmNOpqrSTUVwxyzabCDE1f2", cx);
            editor
        });

        cx.observe(&state, |_, _, cx| {
            cx.notify();
        })
        .detach();

        let load_credentials_task = Some(cx.spawn_in(window, {
            let state = state.clone();
            async move |this, cx| {
                if let Some(task) = state
                    .update(cx, |state, cx| state.authenticate(cx))
                    .log_err()
                {
                    // We don't log an error, because "not signed in" is also an error.
                    let _ = task.await;
                }

                this.update(cx, |this, cx| {
                    this.load_credentials_task = None;
                    cx.notify();
                })
                .log_err();
            }
        }));

        Self {
            api_key_editor,
            state,
            load_credentials_task,
        }
    }

    fn save_api_key(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let api_key = self.api_key_editor.read(cx).text(cx);
        if api_key.is_empty() {
            return;
        }

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state
                .update(cx, |state, cx| state.set_api_key(api_key, cx))?
                .await
        })
        .detach_and_log_err(cx);

        cx.notify();
    }

    fn reset_api_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.api_key_editor
            .update(cx, |editor, cx| editor.set_text("", window, cx));

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state.update(cx, |state, cx| state.reset_api_key(cx))?.await
        })
        .detach_and_log_err(cx);

        cx.notify();
    }

    fn render_api_key_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let text_style = TextStyle {
            color: cx.theme().colors().text,
            font_family: settings.ui_font.family.clone(),
            font_features: settings.ui_font.features.clone(),
            font_fallbacks: settings.ui_font.fallbacks.clone(),
            font_size: rems(0.875).into(),
            font_weight: settings.ui_font.weight,
            font_style: FontStyle::Normal,
            line_height: relative(1.3),
            white_space: WhiteSpace::Normal,
            ..Default::default()
        };
        EditorElement::new(
            &self.api_key_editor,
            EditorStyle {
                background: cx.theme().colors().editor_background,
                local_player: cx.theme().players().local(),
                text: text_style,
                ..Default::default()
            },
        )
    }

    fn should_render_editor(&self, cx: &mut Context<Self>) -> bool {
        !self.state.read(cx).is_authenticated()
    }
}

impl Render for ConfigurationView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let env_var_set = self.state.read(cx).api_key_from_env;

        if self.load_credentials_task.is_some() {
            div().child(Label::new("Loading credentials...")).into_any()
        } else if self.should_render_editor(cx) {
            v_flex()
                .size_full()
                .on_action(cx.listener(Self::save_api_key))
                .child(Label::new("To use Zed's assistant with Mistral, you need to add an API key. Follow these steps:"))
                .child(
                    List::new()
                        .child(InstructionListItem::new(
                            "Create one by visiting",
                            Some("Mistral's console"),
                            Some("https://console.mistral.ai/api-keys"),
                        ))
                        .child(InstructionListItem::text_only(
                            "Ensure your Mistral account has credits",
                        ))
                        .child(InstructionListItem::text_only(
                            "Paste your API key below and hit enter to start using the assistant",
                        )),
                )
                .child(
                    h_flex()
                        .w_full()
                        .my_2()
                        .px_2()
                        .py_1()
                        .bg(cx.theme().colors().editor_background)
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .rounded_sm()
                        .child(self.render_api_key_editor(cx)),
                )
                .child(
                    Label::new(
                        format!("You can also assign the {MISTRAL_API_KEY_VAR} environment variable and restart Zed."),
                    )
                    .size(LabelSize::Small).color(Color::Muted),
                )
                .into_any()
        } else {
            h_flex()
                .mt_1()
                .p_1()
                .justify_between()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().background)
                .child(
                    h_flex()
                        .gap_1()
                        .child(Icon::new(IconName::Check).color(Color::Success))
                        .child(Label::new(if env_var_set {
                            format!("API key set in {MISTRAL_API_KEY_VAR} environment variable.")
                        } else {
                            "API key configured.".to_string()
                        })),
                )
                .child(
                    Button::new("reset-key", "Reset Key")
                        .label_size(LabelSize::Small)
                        .icon(Some(IconName::Trash))
                        .icon_size(IconSize::Small)
                        .icon_position(IconPosition::Start)
                        .disabled(env_var_set)
                        .when(env_var_set, |this| {
                            this.tooltip(Tooltip::text(format!("To reset your API key, unset the {MISTRAL_API_KEY_VAR} environment variable.")))
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.reset_api_key(window, cx))),
                )
                .into_any()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use language_model::{LanguageModelImage, LanguageModelRequestMessage, MessageContent};

    #[test]
    fn test_into_mistral_basic_conversion() {
        let request = LanguageModelRequest {
            messages: vec![
                LanguageModelRequestMessage {
                    role: Role::System,
                    content: vec![MessageContent::Text("System prompt".into())],
                    cache: false,
                },
                LanguageModelRequestMessage {
                    role: Role::User,
                    content: vec![MessageContent::Text("Hello".into())],
                    cache: false,
                },
            ],
            temperature: Some(0.5),
            tools: vec![],
            tool_choice: None,
            thread_id: None,
            prompt_id: None,
            intent: None,
            mode: None,
            stop: vec![],
            thinking_allowed: true,
        };

        let mistral_request = into_mistral(request, "mistral-small-latest".into(), None);

        assert_eq!(mistral_request.model, "mistral-small-latest");
        assert_eq!(mistral_request.temperature, Some(0.5));
        assert_eq!(mistral_request.messages.len(), 2);
        assert!(mistral_request.stream);
    }

    #[test]
    fn test_into_mistral_with_image() {
        let request = LanguageModelRequest {
            messages: vec![LanguageModelRequestMessage {
                role: Role::User,
                content: vec![
                    MessageContent::Text("What's in this image?".into()),
                    MessageContent::Image(LanguageModelImage {
                        source: "base64data".into(),
                        size: Default::default(),
                    }),
                ],
                cache: false,
            }],
            tools: vec![],
            tool_choice: None,
            temperature: None,
            thread_id: None,
            prompt_id: None,
            intent: None,
            mode: None,
            stop: vec![],
            thinking_allowed: true,
        };

        let mistral_request = into_mistral(request, "pixtral-12b-latest".into(), None);

        assert_eq!(mistral_request.messages.len(), 1);
        assert!(matches!(
            &mistral_request.messages[0],
            mistral::RequestMessage::User {
                content: mistral::MessageContent::Multipart { .. }
            }
        ));

        if let mistral::RequestMessage::User {
            content: mistral::MessageContent::Multipart { content },
        } = &mistral_request.messages[0]
        {
            assert_eq!(content.len(), 2);
            assert!(matches!(
                &content[0],
                mistral::MessagePart::Text { text } if text == "What's in this image?"
            ));
            assert!(matches!(
                &content[1],
                mistral::MessagePart::ImageUrl { image_url } if image_url.starts_with("data:image/png;base64,")
            ));
        }
    }
}
