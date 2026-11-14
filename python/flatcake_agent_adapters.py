"""flatcake_agent_adapters — optional integrations with AI agent frameworks.

Each adapter provides observability and workload-identity integration only.
They do NOT intercept KV cache contents, inject prompts, capture conversations,
or alter framework execution semantics. Installing the framework is optional;
the adapter degrades gracefully when the framework is not installed.

Usage (LangChain example):
    from flatcake_sdk import FlatCake
    from flatcake_agent_adapters import LangChainCacheObserver

    fc = FlatCake()
    observer = LangChainCacheObserver(fc, agent_id="my-agent")
    # ... use observer as a callback handler in your LangChain agent ...
"""

import os
import time
from typing import Optional


def _project_id() -> str:
    from flatcake_sdk_import_helper import _current_project_id
    return _current_project_id()


class _BaseAdapter:
    """Shared registration and telemetry logic."""

    def __init__(self, client, agent_id: str, project_id: str = ""):
        self._client = client
        self._agent_id = agent_id
        self._project_id = project_id or os.path.basename(os.getcwd())
        self._runtime_id = f"{self.FRAMEWORK}:{agent_id}"
        self._registered = False

    def _ensure_registered(self):
        if self._registered:
            return
        try:
            self._client.register_runtime(
                runtime_id=self._runtime_id,
                project_id=self._project_id,
                pid=os.getpid(),
                name=self.FRAMEWORK,
                version=self._framework_version(),
                capabilities=1,  # STATUS only
            )
            self._registered = True
        except Exception:
            pass  # service not running — adapter degrades gracefully

    def _framework_version(self) -> str:
        try:
            mod = __import__(self.FRAMEWORK)
            return getattr(mod, "__version__", "unknown")
        except ImportError:
            return "not_installed"

    def _record(self, event: str, data: dict):
        self._ensure_registered()
        self._client.heartbeat(self._runtime_id)


# ---- LangChain ----

class LangChainCacheObserver(_BaseAdapter):
    """Optional callback handler for LangChain agents.

    Records agent identity and session activity to FlatCake's registry.
    Does NOT intercept KV cache contents or inject prompts.

    Usage:
        from flatcake_agent_adapters import LangChainCacheObserver
        observer = LangChainCacheObserver(client, agent_id="research-agent")
        agent_executor = AgentExecutor(..., callbacks=[observer])
    """

    FRAMEWORK = "langchain"

    def on_chain_start(self, serialized, inputs, **kwargs):
        self._record("chain_start", inputs)

    def on_chain_end(self, outputs, **kwargs):
        self._record("chain_end", outputs)

    def on_llm_start(self, serialized, prompts, **kwargs):
        self._record("llm_start", {})

    def on_llm_end(self, response, **kwargs):
        self._record("llm_end", {})

    def on_agent_action(self, action, **kwargs):
        self._record("agent_action", {})

    def on_agent_finish(self, finish, **kwargs):
        self._record("agent_finish", {})

    def _record(self, event: str, data: dict):
        try:
            self._ensure_registered()
            self._client.heartbeat(self._runtime_id)
        except Exception:
            pass  # adapter degrades gracefully when the service is down


# ---- CrewAI ----

class CrewAICacheObserver(_BaseAdapter):
    """Optional hook for CrewAI crews: registers the crew and its agents."""

    FRAMEWORK = "crewai"

    def register_crew(self, crew):
        """Register a CrewAI Crew instance's agents with FlatCake."""
        try:
            for agent in crew.agents:
                agent_id = f"{self._agent_id}:{getattr(agent, 'role', 'unknown')}"
                self._client.register_runtime(
                    runtime_id=f"crewai:{agent_id}",
                    project_id=self._project_id,
                    pid=os.getpid(),
                    name=self.FRAMEWORK,
                    version=self._framework_version(),
                    capabilities=1,
                )
        except Exception:
            pass  # degrade gracefully

    def _record(self, event: str, data: dict):
        try:
            self._ensure_registered()
            self._client.heartbeat(self._runtime_id)
        except Exception:
            pass

    def on_crew_start(self, crew):
        self.register_crew(crew)

    def on_task_complete(self, task_output):
        self._record("task_complete", {})


# ---- AutoGen ----

class AutoGenCacheObserver(_BaseAdapter):
    """Optional hook for Microsoft AutoGen: registers agent sessions."""

    FRAMEWORK = "autogen"

    def register_agents(self, agent_names: list):
        try:
            for name in agent_names:
                self._client.register_runtime(
                    runtime_id=f"autogen:{self._agent_id}:{name}",
                    project_id=self._project_id,
                    pid=os.getpid(),
                    name=self.FRAMEWORK,
                    version=self._framework_version(),
                    capabilities=1,
                )
        except Exception:
            pass

    def on_message(self, sender, message):
        self._record("message", {})

    def _record(self, event: str, data: dict):
        try:
            self._ensure_registered()
            self._client.heartbeat(self._runtime_id)
        except Exception:
            pass


# ---- OpenAI Agents SDK ----

class OpenAIAgentsCacheObserver(_BaseAdapter):
    """Optional tracing hook for the OpenAI Agents SDK."""

    FRAMEWORK = "openai_agents"

    def on_agent_start(self, agent_name: str, input_data):
        self._record("agent_start", {"agent": agent_name, "input_len": len(str(input_data))})

    def on_agent_end(self, agent_name: str, output):
        self._record("agent_end", {"agent": agent_name})

    def _record(self, event: str, data: dict):
        try:
            self._ensure_registered()
            self._client.heartbeat(self._runtime_id)
        except Exception:
            pass
