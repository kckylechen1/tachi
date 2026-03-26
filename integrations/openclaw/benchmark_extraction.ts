#!/usr/bin/env npx tsx
/**
 * Memory Extraction Benchmark: Qwen3-8B vs GLM-4-9B
 *
 * 用途：对比两个模型在记忆提取任务上的表现，输出统一 JSON 报告。
 *
 * 运行命令：
 *   cd /Users/kckylechen/Desktop/Sigil/integrations/openclaw
 *   npx tsx benchmark_extraction.ts
 *
 * 输出文件：
 *   benchmark_extraction_result.json (在当前目录)
 *
 * 成功条件：
 *   - 进程 exit code = 0
 *   - 结果文件存在且包含有效 JSON
 *
 * 环境变量：
 *   - SILICONFLOW_API_KEY: 必需，SiliconFlow API 密钥
 */

import fs from "node:fs";
import path from "node:path";
import https from "node:https";
import http from "node:http";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

// ============================================================================
// 类型定义
// ============================================================================

type TestCase = {
  id: string;
  input: string;
  expected_facts: string[]; // 期望提取的关键事实（模糊匹配）
  category: string;
};

type ModelResult = {
  model: string;
  test_id: string;
  success: boolean;
  latency_ms: number;
  extracted_facts: string[];
  error?: string;
  raw_response?: string;
};

type BenchmarkReport = {
  timestamp: string;
  models: {
    qwen3_8b: ModelMetrics;
    glm_4_9b: ModelMetrics;
  };
  comparison: {
    latency_diff_ms: number;
    recall_diff_pct: number;
    precision_diff_pct: number;
    winner: "qwen3_8b" | "glm_4_9b" | "tie";
  };
  test_cases: TestCase[];
  raw_results: {
    qwen3_8b: ModelResult[];
    glm_4_9b: ModelResult[];
  };
  config: {
    api_base_url: string;
    retry_count: number;
    timeout_ms: number;
  };
};

type ModelMetrics = {
  model: string;
  total_tests: number;
  successful: number;
  failed: number;
  recall: number;
  precision: number;
  f1: number;
  avg_latency_ms: number;
  success_rate: number;
};

// ============================================================================
// 配置
// ============================================================================

const CONFIG = {
  api_base_url: "https://api.siliconflow.cn/v1/chat/completions",
  retry_count: 1, // 自动重试 1 次
  timeout_ms: 30000,
  models: {
    qwen3_8b: "Qwen/Qwen3-8B",
    glm_4_9b: "THUDM/glm-4-9b-chat",
  },
};

// ============================================================================
// 测试数据集
// ============================================================================

const TEST_CASES: TestCase[] = [
  {
    id: "pref_001",
    category: "preference",
    input:
      "我平时喜欢喝冰美式，尤其是星巴克的双倍浓缩。不喜欢加糖或牛奶，那样会破坏咖啡原本的味道。",
    expected_facts: ["喜欢喝冰美式", "星巴克双倍浓缩", "不喜欢加糖", "不喜欢加牛奶"],
  },
  {
    id: "fact_002",
    category: "fact",
    input:
      "我的生日是1990年5月15日，出生在北京海淀区。我在清华大学读的计算机专业，2012年毕业。",
    expected_facts: ["生日1990年5月15日", "出生北京海淀区", "清华大学计算机专业", "2012年毕业"],
  },
  {
    id: "dec_003",
    category: "decision",
    input:
      "我决定下周一开始健身计划，每天早上6点起床跑步5公里。如果坚持一个月，就奖励自己买一双新跑鞋。",
    expected_facts: ["下周一开始健身", "每天早上6点", "跑步5公里", "坚持一个月", "奖励新跑鞋"],
  },
  {
    id: "entity_004",
    category: "entity",
    input:
      "张三是我大学室友，现在在字节跳动做后端开发。他老婆叫李四，在阿里巴巴做产品经理。",
    expected_facts: ["张三大学室友", "字节跳动后端开发", "李四老婆", "阿里巴巴产品经理"],
  },
  {
    id: "plan_005",
    category: "planning",
    input:
      "下个月15号我要去上海出差三天，住在陆家嘴的香格里拉酒店。主要任务是和客户谈合作，希望能签下合同。",
    expected_facts: ["下个月15号上海出差", "三天", "陆家嘴香格里拉酒店", "谈合作", "签合同"],
  },
  {
    id: "tech_006",
    category: "technical",
    input:
      "我刚才查了 OpenClaw 的代码，发现它原本是按 JSONL 格式把所有聊天记忆全存到一个文件里，然后通过一个定时任务每天晚上唤醒 ops agent，让 ops 把新条目里的因果关系提炼出来。",
    expected_facts: ["OpenClaw代码", "JSONL格式存储", "定时任务", "每天晚上唤醒", "ops agent", "提炼因果关系"],
  },
  {
    id: "complex_007",
    category: "complex",
    input:
      "我爸今年65岁，退休前是中学校长。他最近身体不太好，有高血压和糖尿病，每个月都要去医院复查。医生建议他少盐少油，多运动。",
    expected_facts: ["父亲65岁", "退休中学校长", "高血压", "糖尿病", "每月医院复查", "少盐少油", "多运动"],
  },
  {
    id: "habit_008",
    category: "habit",
    input:
      "我每天晚上10点左右睡觉，早上6点半起床。睡前习惯看半小时书，通常是技术类的或者历史类的。",
    expected_facts: ["晚上10点睡觉", "早上6点半起床", "睡前看书半小时", "技术类或历史类"],
  },
];

// ============================================================================
// API 调用工具
// ============================================================================

const SYSTEM_PROMPT = `你是一个记忆提取助手。请从用户输入中提取关键事实。

输出格式要求：
1. 每行一个事实，使用简短、精确的陈述句
2. 只提取客观事实，不要添加主观判断
3. 保留关键数字、日期、地点、人名等实体
4. 使用原文中的词汇，不要过度改写
5. 事实应该独立且完整，可以脱离上下文理解

示例输入：
"我平时喜欢喝冰美式，尤其是星巴克的双倍浓缩。"

示例输出：
喜欢喝冰美式
偏爱星巴克双倍浓缩`;

function parseUrl(url: string): { protocol: string; host: string; path: string } {
  const parsed = new URL(url);
  return {
    protocol: parsed.protocol,
    host: parsed.host,
    path: parsed.pathname + parsed.search,
  };
}

async function callModel(
  model: string,
  input: string,
  apiKey: string,
  retryCount: number = CONFIG.retry_count,
): Promise<ModelResult> {
  const startTime = Date.now();
  let lastError: string | undefined;

  for (let attempt = 0; attempt <= retryCount; attempt++) {
    try {
      const result = await doCallModel(model, input, apiKey);
      result.latency_ms = Date.now() - startTime;
      return result;
    } catch (err: any) {
      lastError = err.message || String(err);
      if (attempt < retryCount) {
        // 等待后重试
        await new Promise((resolve) => setTimeout(resolve, 1000 * (attempt + 1)));
      }
    }
  }

  return {
    model,
    test_id: "",
    success: false,
    latency_ms: Date.now() - startTime,
    extracted_facts: [],
    error: lastError,
  };
}

async function doCallModel(model: string, input: string, apiKey: string): Promise<ModelResult> {
  const url = parseUrl(CONFIG.api_base_url);
  const transport = url.protocol === "https:" ? https : http;

  const body = JSON.stringify({
    model,
    messages: [
      { role: "system", content: SYSTEM_PROMPT },
      { role: "user", content: input },
    ],
    temperature: 0.1,
    max_tokens: 500,
  });

  return new Promise((resolve, reject) => {
    const req = transport.request(
      {
        hostname: url.host,
        path: url.path,
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Authorization: `Bearer ${apiKey}`,
        },
        timeout: CONFIG.timeout_ms,
      },
      (res) => {
        let data = "";
        res.on("data", (chunk) => (data += chunk));
        res.on("end", () => {
          try {
            if (res.statusCode !== 200) {
              reject(new Error(`HTTP ${res.statusCode}: ${data}`));
              return;
            }
            const json = JSON.parse(data);
            const content = json.choices?.[0]?.message?.content || "";

            // 解析提取的事实
            const facts = content
              .split("\n")
              .map((line: string) => line.trim())
              .filter((line: string) => line.length > 0 && !line.startsWith("#"));

            resolve({
              model,
              test_id: "",
              success: true,
              latency_ms: 0, // 由调用者设置
              extracted_facts: facts,
              raw_response: content,
            });
          } catch (err: any) {
            reject(new Error(`JSON parse error: ${err.message}`));
          }
        });
      },
    );

    req.on("error", reject);
    req.on("timeout", () => {
      req.destroy();
      reject(new Error("Request timeout"));
    });
    req.write(body);
    req.end();
  });
}

// ============================================================================
// 评测指标计算
// ============================================================================

function fuzzyMatch(extracted: string, expected: string): boolean {
  const ext = extracted.toLowerCase();
  const exp = expected.toLowerCase();

  // 完全包含
  if (ext.includes(exp) || exp.includes(ext)) return true;

  // 关键词匹配（至少 50% 字符匹配）
  const expChars = new Set(exp.replace(/\s+/g, ""));
  const matchCount = [...ext.replace(/\s+/g, "")].filter((c) => expChars.has(c)).length;
  return matchCount >= expChars.size * 0.5;
}

function calculateMetrics(results: ModelResult[], testCases: TestCase[]): ModelMetrics {
  const model = results[0]?.model || "unknown";
  const totalTests = testCases.length;

  let totalRecall = 0;
  let totalPrecision = 0;
  let successfulCount = 0;
  let totalLatency = 0;

  for (const result of results) {
    const testCase = testCases.find((tc) => tc.id === result.test_id);
    if (!testCase) continue;

    totalLatency += result.latency_ms;

    if (!result.success) continue;
    successfulCount++;

    // 计算 recall: 提取到的事实中，有多少期望事实被覆盖
    const matchedExpected = testCase.expected_facts.filter((exp) =>
      result.extracted_facts.some((ext) => fuzzyMatch(ext, exp)),
    );
    const recall = testCase.expected_facts.length > 0
      ? matchedExpected.length / testCase.expected_facts.length
      : 0;

    // 计算 precision: 提取的事实中，有多少与期望相关
    const matchedExtracted = result.extracted_facts.filter((ext) =>
      testCase.expected_facts.some((exp) => fuzzyMatch(ext, exp)),
    );
    const precision = result.extracted_facts.length > 0
      ? matchedExtracted.length / result.extracted_facts.length
      : 0;

    totalRecall += recall;
    totalPrecision += precision;
  }

  const avgRecall = successfulCount > 0 ? totalRecall / successfulCount : 0;
  const avgPrecision = successfulCount > 0 ? totalPrecision / successfulCount : 0;
  const f1 = avgRecall + avgPrecision > 0 ? (2 * avgRecall * avgPrecision) / (avgRecall + avgPrecision) : 0;

  return {
    model,
    total_tests: totalTests,
    successful: successfulCount,
    failed: totalTests - successfulCount,
    recall: Math.round(avgRecall * 1000) / 1000,
    precision: Math.round(avgPrecision * 1000) / 1000,
    f1: Math.round(f1 * 1000) / 1000,
    avg_latency_ms: Math.round(totalLatency / Math.max(successfulCount, 1)),
    success_rate: Math.round((successfulCount / totalTests) * 1000) / 1000,
  };
}

// ============================================================================
// 主程序
// ============================================================================

async function main() {
  console.log("=".repeat(70));
  console.log("  Memory Extraction Benchmark: Qwen3-8B vs GLM-4-9B");
  console.log("=".repeat(70));
  console.log();

  // 检查 API Key
  const apiKey = process.env.SILICONFLOW_API_KEY;
  if (!apiKey || apiKey === "your_siliconflow_api_key_here") {
    console.error("❌ 错误：SILICONFLOW_API_KEY 环境变量未设置或无效");
    console.error();
    console.error("修复命令：");
    console.error("  export SILICONFLOW_API_KEY='你的API密钥'");
    console.error();
    console.error("获取 API Key：");
    console.error("  1. 访问 https://cloud.siliconflow.cn/");
    console.error("  2. 注册/登录账号");
    console.error("  3. 在「API密钥」页面创建密钥");
    process.exit(1);
  }

  console.log(`✅ API Key 已设置 (长度: ${apiKey.length} 字符)`);
  console.log(`📊 测试用例数量: ${TEST_CASES.length}`);
  console.log(`🔄 自动重试次数: ${CONFIG.retry_count}`);
  console.log();

  // 存储结果
  const qwenResults: ModelResult[] = [];
  const glmResults: ModelResult[] = [];

  // 逐个测试
  for (const tc of TEST_CASES) {
    console.log(`\n📝 测试 [${tc.id}] (${tc.category}): "${tc.input.slice(0, 40)}..."`);

    // 测试 Qwen3-8B
    process.stdout.write("   Qwen3-8B ... ");
    const qwenResult = await callModel(CONFIG.models.qwen3_8b, tc.input, apiKey);
    qwenResult.test_id = tc.id;
    qwenResults.push(qwenResult);
    console.log(
      qwenResult.success
        ? `✅ ${qwenResult.latency_ms}ms, ${qwenResult.extracted_facts.length} facts`
        : `❌ ${qwenResult.error}`,
    );

    // 测试 GLM-4-9B
    process.stdout.write("   GLM-4-9B  ... ");
    const glmResult = await callModel(CONFIG.models.glm_4_9b, tc.input, apiKey);
    glmResult.test_id = tc.id;
    glmResults.push(glmResult);
    console.log(
      glmResult.success
        ? `✅ ${glmResult.latency_ms}ms, ${glmResult.extracted_facts.length} facts`
        : `❌ ${glmResult.error}`,
    );
  }

  // 计算指标
  const qwenMetrics = calculateMetrics(qwenResults, TEST_CASES);
  const glmMetrics = calculateMetrics(glmResults, TEST_CASES);

  // 生成报告
  const report: BenchmarkReport = {
    timestamp: new Date().toISOString(),
    models: {
      qwen3_8b: qwenMetrics,
      glm_4_9b: glmMetrics,
    },
    comparison: {
      latency_diff_ms: qwenMetrics.avg_latency_ms - glmMetrics.avg_latency_ms,
      recall_diff_pct: Math.round((qwenMetrics.recall - glmMetrics.recall) * 1000) / 10,
      precision_diff_pct: Math.round((qwenMetrics.precision - glmMetrics.precision) * 1000) / 10,
      winner:
        qwenMetrics.f1 > glmMetrics.f1
          ? "qwen3_8b"
          : glmMetrics.f1 > qwenMetrics.f1
            ? "glm_4_9b"
            : "tie",
    },
    test_cases: TEST_CASES,
    raw_results: {
      qwen3_8b: qwenResults,
      glm_4_9b: glmResults,
    },
    config: {
      api_base_url: CONFIG.api_base_url,
      retry_count: CONFIG.retry_count,
      timeout_ms: CONFIG.timeout_ms,
    },
  };

  // 输出结果文件
  const outputPath = path.join(__dirname, "benchmark_extraction_result.json");
  fs.writeFileSync(outputPath, JSON.stringify(report, null, 2));

  // 打印摘要
  console.log("\n" + "=".repeat(70));
  console.log("  评测结果摘要");
  console.log("=".repeat(70));
  console.log();
  console.log("  指标                 Qwen3-8B      GLM-4-9B      差异");
  console.log("  " + "-".repeat(60));
  console.log(
    `  Recall              ${(qwenMetrics.recall * 100).toFixed(1).padStart(6)}%       ${(glmMetrics.recall * 100).toFixed(1).padStart(6)}%       ${report.comparison.recall_diff_pct.toFixed(1)}%`,
  );
  console.log(
    `  Precision           ${(qwenMetrics.precision * 100).toFixed(1).padStart(6)}%       ${(glmMetrics.precision * 100).toFixed(1).padStart(6)}%       ${report.comparison.precision_diff_pct.toFixed(1)}%`,
  );
  console.log(
    `  F1 Score            ${qwenMetrics.f1.toFixed(3).padStart(6)}       ${glmMetrics.f1.toFixed(3).padStart(6)}`,
  );
  console.log(
    `  Avg Latency         ${String(qwenMetrics.avg_latency_ms + "ms").padStart(6)}       ${String(glmMetrics.avg_latency_ms + "ms").padStart(6)}       ${report.comparison.latency_diff_ms > 0 ? "+" : ""}${report.comparison.latency_diff_ms}ms`,
  );
  console.log(
    `  Success Rate        ${(qwenMetrics.success_rate * 100).toFixed(0).padStart(6)}%       ${(glmMetrics.success_rate * 100).toFixed(0).padStart(6)}%`,
  );
  console.log();
  console.log(`  🏆 胜出: ${report.comparison.winner.toUpperCase()}`);
  console.log();
  console.log(`📄 详细报告已保存到: ${outputPath}`);
  console.log("=".repeat(70));

  // 强制成功条件检查
  if (!fs.existsSync(outputPath)) {
    console.error("❌ 错误：结果文件未生成");
    process.exit(1);
  }

  // 验证 JSON 有效性
  try {
    JSON.parse(fs.readFileSync(outputPath, "utf-8"));
  } catch (e) {
    console.error("❌ 错误：结果文件 JSON 无效");
    process.exit(1);
  }

  // 成功退出
  process.exit(0);
}

main().catch((err) => {
  console.error("❌ 未捕获的错误:", err);
  process.exit(1);
});
