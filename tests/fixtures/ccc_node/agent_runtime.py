"""Provider-neutral contracts for agent runtimes, sessions, and events.

This module intentionally contains no provider adapters.  It defines the seam a
future runtime can implement without exposing provider SDK objects to bridge
callers.
"""

from __future__ import annotations

from collections.abc import AsyncIterator, Awaitable, Callable, Mapping, Sequence
from dataclasses import dataclass
from enum import Enum
import math
from types import MappingProxyType
from typing import Literal, Protocol, TypeAlias

JsonValue: TypeAlias = (
    None
    | bool
    | int
    | float
    | str
    | list["JsonValue"]
    | tuple["JsonValue", ...]
    | Mapping[str, "JsonValue"]
)


def freeze_json(value: JsonValue) -> JsonValue:
    """Return a recursively immutable snapshot of a JSON-compatible value."""

    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, Mapping):
        return MappingProxyType({key: freeze_json(item) for key, item in value.items()})
    if isinstance(value, (list, tuple)):
        return tuple(freeze_json(item) for item in value)
    raise TypeError(f"unsupported JSON value: {type(value).__name__}")


class ApprovalDecision(str, Enum):
    """An explicit response to a provider-neutral approval request."""

    ALLOW = "allow"
    DENY = "deny"


@dataclass(frozen=True, slots=True)
class TextDeltaEvent:
    """A non-empty increment of user-visible assistant text."""

    text: str
    kind: Literal["text_delta"] = "text_delta"

    def __post_init__(self) -> None:
        if not self.text:
            raise ValueError("text delta must not be empty")


@dataclass(frozen=True, slots=True)
class MessageCompletedEvent:
    """One user-visible assistant message finished within the current turn."""

    kind: Literal["message_completed"] = "message_completed"


@dataclass(frozen=True, slots=True)
class ReasoningDeltaEvent:
    """A non-empty increment of provider-normalized reasoning text."""

    text: str
    kind: Literal["reasoning_delta"] = "reasoning_delta"

    def __post_init__(self) -> None:
        if not self.text:
            raise ValueError("reasoning delta must not be empty")


@dataclass(frozen=True, slots=True)
class ToolStartedEvent:
    """A provider tool item began execution."""

    tool_call_id: str
    tool_name: str
    arguments: Mapping[str, JsonValue]
    kind: Literal["tool_started"] = "tool_started"

    def __post_init__(self) -> None:
        if not self.tool_call_id:
            raise ValueError("tool call id must not be empty")
        if not self.tool_name:
            raise ValueError("tool name must not be empty")
        object.__setattr__(
            self,
            "arguments",
            MappingProxyType({key: freeze_json(value) for key, value in self.arguments.items()}),
        )


@dataclass(frozen=True, slots=True)
class ToolCompletedEvent:
    """A provider tool item finished execution."""

    tool_call_id: str
    tool_name: str
    result: JsonValue
    success: bool
    kind: Literal["tool_completed"] = "tool_completed"

    def __post_init__(self) -> None:
        if not self.tool_call_id:
            raise ValueError("tool call id must not be empty")
        if not self.tool_name:
            raise ValueError("tool name must not be empty")
        object.__setattr__(self, "result", freeze_json(self.result))


@dataclass(frozen=True, slots=True)
class ApprovalRequestEvent:
    """A request that cannot proceed without an explicit approval decision."""

    request_id: str
    action: str
    arguments: Mapping[str, JsonValue]
    description: str
    kind: Literal["approval_request"] = "approval_request"

    def __post_init__(self) -> None:
        if not self.request_id:
            raise ValueError("approval request id must not be empty")
        if not self.action:
            raise ValueError("approval action must not be empty")
        if not self.description:
            raise ValueError("approval description must not be empty")
        object.__setattr__(
            self,
            "arguments",
            MappingProxyType({key: freeze_json(value) for key, value in self.arguments.items()}),
        )


@dataclass(frozen=True, slots=True)
class DelegatedTaskLifecycleEvent:
    """Body-free snapshot of delegated work owned by the active provider turn.

    Provider task identifiers, descriptions, prompts, and results deliberately
    stop at the adapter.  The orchestrator needs only this aggregate lifecycle
    state to distinguish a healthy delegated run from a vanished terminal
    frame.  ``oldest_age_seconds`` is measured by the adapter's monotonic clock
    and is absent exactly when no delegated task remains active.
    """

    active_count: int
    oldest_age_seconds: float | None
    activity: Literal["started", "updated", "terminal"]
    kind: Literal["delegated_task_lifecycle"] = "delegated_task_lifecycle"

    def __post_init__(self) -> None:
        if (
            isinstance(self.active_count, bool)
            or not isinstance(self.active_count, int)
            or self.active_count < 0
        ):
            raise ValueError("delegated task active count must be non-negative")
        if self.active_count == 0:
            if self.oldest_age_seconds is not None:
                raise ValueError("idle delegated task state cannot carry an oldest age")
            return
        age = self.oldest_age_seconds
        if (
            age is None
            or isinstance(age, bool)
            or not isinstance(age, (int, float))
            or not math.isfinite(age)
            or age < 0
        ):
            raise ValueError("active delegated task state requires a non-negative age")


@dataclass(frozen=True, slots=True)
class CompletionEvent:
    """The provider has finished generating the current turn."""

    stop_reason: str
    kind: Literal["completion"] = "completion"

    def __post_init__(self) -> None:
        if not self.stop_reason:
            raise ValueError("completion stop reason must not be empty")


@dataclass(frozen=True, slots=True)
class ResultEvent:
    """The normalized result of a successfully completed turn."""

    result: JsonValue
    kind: Literal["result"] = "result"

    def __post_init__(self) -> None:
        object.__setattr__(self, "result", freeze_json(self.result))


@dataclass(frozen=True, slots=True)
class ErrorEvent:
    """A provider-normalized terminal runtime error."""

    code: str
    message: str
    retryable: bool = False
    kind: Literal["error"] = "error"

    def __post_init__(self) -> None:
        if not self.code:
            raise ValueError("error code must not be empty")
        if not self.message:
            raise ValueError("error message must not be empty")


AgentEvent: TypeAlias = (
    TextDeltaEvent
    | MessageCompletedEvent
    | ReasoningDeltaEvent
    | ToolStartedEvent
    | ToolCompletedEvent
    | ApprovalRequestEvent
    | DelegatedTaskLifecycleEvent
    | CompletionEvent
    | ResultEvent
    | ErrorEvent
)
ApprovalHandler: TypeAlias = Callable[[ApprovalRequestEvent], Awaitable[ApprovalDecision]]


async def deny_approval(_request: ApprovalRequestEvent) -> ApprovalDecision:
    """Fail-closed approval handler used whenever a caller supplies none."""

    return ApprovalDecision.DENY


@dataclass(frozen=True, slots=True)
class AsyncCompletionCapability:
    """Body-free protocol boundary for out-of-turn completion delivery.

    This describes only provider surfaces the running adapter can safely use.
    It is not an event and carries no provider, thread, task, or message payload.
    ``protocol_version=None`` means the server did not negotiate a version that
    can be pinned to a supported async-delivery contract.
    """

    provider: str
    state: Literal["supported", "degraded"]
    protocol_version: str | None
    notification_method: str | None
    recovery_method: str | None
    ownership_scope: Literal["exact_active_turn", "detached_task"]
    supports_durable_delivery: bool
    reason_code: str

    def __post_init__(self) -> None:
        if self.state not in {"supported", "degraded"}:
            raise ValueError("async completion state is invalid")
        if self.ownership_scope not in {"exact_active_turn", "detached_task"}:
            raise ValueError("async completion ownership scope is invalid")
        for name in ("provider", "reason_code"):
            value = getattr(self, name)
            if not value or value.strip() != value or len(value) > 128:
                raise ValueError(f"async completion {name} is invalid")
        for name in ("protocol_version", "notification_method", "recovery_method"):
            value = getattr(self, name)
            if value is not None and (
                not value or value.strip() != value or len(value) > 128
            ):
                raise ValueError(f"async completion {name} is invalid")
        if self.state == "supported":
            if (
                not self.supports_durable_delivery
                or self.protocol_version is None
                or self.notification_method is None
                or self.recovery_method is None
                or self.ownership_scope != "detached_task"
            ):
                raise ValueError("supported async completion capability is incomplete")
        elif self.supports_durable_delivery:
            raise ValueError("degraded async completion cannot enable durable delivery")


class AsyncCompletionRuntime(Protocol):
    """Optional provider-neutral async-completion capability seam."""

    def async_completion_capability(self) -> AsyncCompletionCapability: ...


@dataclass(frozen=True, slots=True)
class SessionRequest:
    """Provider-neutral inputs for starting or resuming an agent session."""

    working_directory: str
    session_id: str | None = None
    model: str | None = None
    effort: str | None = None
    approval_policy: str | None = None
    approvals_reviewer: str | None = None
    sandbox_policy: Mapping[str, JsonValue] | None = None
    memory_environment: Mapping[str, str] | None = None

    def __post_init__(self) -> None:
        if not self.working_directory:
            raise ValueError("working directory must not be empty")
        if self.effort is not None and not self.effort:
            raise ValueError("effort must not be empty")
        if self.approval_policy is not None and not self.approval_policy:
            raise ValueError("approval policy must not be empty")
        if self.approvals_reviewer is not None and not self.approvals_reviewer:
            raise ValueError("approvals reviewer must not be empty")
        if self.sandbox_policy is not None:
            frozen_sandbox = freeze_json(self.sandbox_policy)
            if not isinstance(frozen_sandbox, Mapping) or not frozen_sandbox:
                raise ValueError("sandbox policy must not be empty")
            object.__setattr__(self, "sandbox_policy", frozen_sandbox)
        if self.memory_environment is not None:
            if not isinstance(self.memory_environment, Mapping) or not self.memory_environment:
                raise ValueError("memory environment must not be empty")
            environment: dict[str, str] = {}
            for name, value in self.memory_environment.items():
                if not isinstance(name, str) or not name or "\x00" in name:
                    raise ValueError("memory environment name is invalid")
                if not isinstance(value, str) or "\x00" in value:
                    raise ValueError("memory environment value is invalid")
                environment[name] = value
            object.__setattr__(self, "memory_environment", MappingProxyType(environment))
        if self.session_id == "":
            raise ValueError("session id must not be empty")
        if self.model == "":
            raise ValueError("model must not be empty")


@dataclass(frozen=True, slots=True)
class ModelInfo:
    """A model offered by an agent runtime."""

    id: str
    display_name: str
    default_reasoning_effort: str | None = None
    supported_reasoning_efforts: Sequence[str] = ()
    is_default: bool = False

    def __post_init__(self) -> None:
        if not self.id:
            raise ValueError("model id must not be empty")
        if not self.display_name:
            raise ValueError("model display name must not be empty")
        object.__setattr__(
            self, "supported_reasoning_efforts", tuple(self.supported_reasoning_efforts)
        )


@dataclass(frozen=True, slots=True)
class SessionSummary:
    """Provider-neutral metadata for one resumable session."""

    id: str
    title: str | None = None
    preview: str | None = None
    updated_at: float | None = None
    cwd: str | None = None
    model: str | None = None

    def __post_init__(self) -> None:
        if not self.id:
            raise ValueError("session summary id must not be empty")


@dataclass(frozen=True, slots=True)
class SessionHistoryMessage:
    """One user-visible message from a stored session."""

    role: Literal["user", "assistant"]
    content: str
    timestamp: str | None = None

    def __post_init__(self) -> None:
        if self.role not in {"user", "assistant"}:
            raise ValueError("session history role must be user or assistant")
        if not self.content:
            raise ValueError("session history content must not be empty")


@dataclass(frozen=True, slots=True)
class SessionHistory:
    """Bounded user-visible history for one provider session."""

    session_id: str
    messages: Sequence[SessionHistoryMessage]

    def __post_init__(self) -> None:
        if not self.session_id:
            raise ValueError("session history id must not be empty")
        object.__setattr__(self, "messages", tuple(self.messages))


class SessionBrowser(Protocol):
    """Optional provider-neutral capability for stored-session browsing."""

    @property
    def supports_session_browsing(self) -> bool: ...

    async def list_sessions(self, *, limit: int = 10) -> Sequence[SessionSummary]: ...

    async def read_session(self, session_id: str, *, limit: int = 5) -> SessionHistory: ...


class AgentSession(Protocol):
    """A live provider-neutral agent session."""

    @property
    def session_id(self) -> str:
        """Stable identifier for this session."""
        ...

    def send_turn(
        self,
        message: str,
        *,
        approval_handler: ApprovalHandler = deny_approval,
    ) -> AsyncIterator[AgentEvent]:
        """Send one turn and return its normalized event stream.

        Omitting ``approval_handler`` is always fail-closed: every approval
        request receives ``ApprovalDecision.DENY``. Provider adapters must
        preserve this default instead of delegating omission to an SDK default.
        """
        ...

    async def interrupt(self) -> None:
        """Interrupt in-flight work for this session."""
        ...


class AgentRuntime(Protocol):
    """Factory and model-discovery contract implemented by agent providers."""

    async def start_or_resume(self, request: SessionRequest) -> AgentSession:
        """Start a new session or resume ``request.session_id`` when provided."""
        ...

    async def list_models(self) -> Sequence[ModelInfo]:
        """Return models available from this runtime."""
        ...
