import { defaultConfig } from "./config.js";
import { memoryHybridBridgePlugin } from "./index.js";

async function runTest() {
  const pluginApi: any = {
    pluginConfig: defaultConfig,
    resolvePath: (p: string) => p,
    logger: {
      info: console.log,
      warn: console.warn,
    },
    tools: {},
    registerTool: function (tool: any) {
      this.tools[tool.name] = tool;
    },
    registerService: () => {},
    on: () => {},
  };

  memoryHybridBridgePlugin.register(pluginApi);

  console.log("Registered tools:", Object.keys(pluginApi.tools));

  // Perform search (this will trigger JSONL migration first!)
  const searchTool = pluginApi.tools["memory_search"];
  if (searchTool) {
    const result = await searchTool.execute("test-123", { query: "规划文档", maxResults: 3 });
    console.log("Search Result:");
    console.log(JSON.stringify(result, null, 2));
  }
}

runTest().catch(console.error);
