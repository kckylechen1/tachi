import os
import asyncio
import httpx
import time

API_KEY = os.environ.get("ZHIPU_API_KEY", "878bd1ce27204099b9e16aa138d80843.6x5sYr8IOGsJUWgd")

async def test_summary(model: str, text: str, endpoint: str):
    start = time.time()
    try:
        async with httpx.AsyncClient() as client:
            resp = await client.post(
                endpoint,
                headers={
                    "Authorization": f"Bearer {API_KEY}",
                    "Content-Type": "application/json",
                },
                json={
                    "model": model,
                    "temperature": 0.1,
                    # Provide enough tokens to allow reasoning models to finish
                    "max_tokens": 800,
                    "messages": [
                        {
                            "role": "system",
                            "content": "你是一个摘要助手。请将用户的内容高度概括为10-30个字的一句陈述句，不要包含主观建议，只要事实摘要。"
                        },
                        {
                            "role": "user",
                            "content": text
                        }
                    ]
                },
                timeout=15.0
            )
            resp.raise_for_status()
            data = resp.json()
            # Handle potential reasoning output just in case
            if "reasoning_content" in data["choices"][0]["message"]:
                 content = data["choices"][0]["message"]["content"]
                 print(f"[{model}] 包含推理!")
            else:
                 content = data["choices"][0]["message"]["content"]
            
            elapsed = time.time() - start
            tag = "[CODING]" if "coding" in endpoint else "[STANDARD]"
            print(f"{tag}[{model}] 耗时 {elapsed:.2f}s:\n => {content}\n")
    except Exception as e:
        elapsed = time.time() - start
        tag = "[CODING]" if "coding" in endpoint else "[STANDARD]"
        print(f"{tag}[{model}] 异常 ({elapsed:.2f}s): {str(e)}\n")

TEST_TEXTS = ["我刚才查了 OpenClaw 的代码，发现它原本是按 JSONL 格式把所有聊天记忆全存到一个文件里，然后通过一个定时任务每天晚上唤醒 ops agent，让 ops 把新条目里的因果关系提炼出来。现在我们要把它换成 SQLite。"]

async def main():
    print("开始探测 coding 端点全量可用模型...\n")
    models_to_test = [
        "glm-5", "glm-4.6", "glm-4.7", "glm-4.5-air", "glm-4.5-airx",
        "glm-4-long", "glm-4.7-flash", "glm-4.7-flashx", "glm-4-flashx-250414",
        "glm-4.5-flash", "glm-4-flash", "glm-4v-flash", "glm-4-air", "glm-4-airx"
    ]
    
    endpoint = "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions"
    t = TEST_TEXTS[0]
    
    print(f"=== 并发测试 ===\nURL: {endpoint}\n")
    tasks = [test_summary(m, t, endpoint) for m in models_to_test]
    for task in tasks:
        await task
        await asyncio.sleep(1.0) # Sleep to avoid strict rate limits on heavy models

if __name__ == "__main__":
    asyncio.run(main())
