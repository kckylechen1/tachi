import asyncio
import server
import os

async def test():
    print("\nTesting list_memories...")
    res = await server._list_memories({"path": "/"})
    print(res[0].text)

    print("\nTesting search_memory...")
    search_res = await server._search_memory({"query": "sky blue openclaw", "top_k": 2})
    print(search_res[0].text)

if __name__ == "__main__":
    asyncio.run(test())
