/**
 * Minimal type declarations for the OpenClaw Plugin SDK.
 * Replace with the official SDK types when available.
 */
declare module "openclaw/plugin-sdk" {
  export interface OpenClawPluginApi {
    pluginConfig: unknown;
    logger: {
      info: (...args: any[]) => void;
      warn: (...args: any[]) => void;
      error: (...args: any[]) => void;
      debug: (...args: any[]) => void;
    };
    resolvePath(relativePath: string): string;
    registerTool(tool: {
      name: string;
      label: string;
      description: string;
      parameters: unknown;
      execute: (
        toolCallId: string,
        params: unknown,
        signal: AbortSignal,
        context: unknown,
      ) => Promise<{
        content: Array<{ type: string; text: string }>;
        details?: Record<string, unknown>;
      }>;
    }): void;
    on(
      event: "before_agent_start" | "agent_end",
      handler: (event: any, ctx: any) => Promise<any>,
    ): void;
    registerService(service: {
      id: string;
      start: () => void;
      stop: () => void;
    }): void;
  }
}
