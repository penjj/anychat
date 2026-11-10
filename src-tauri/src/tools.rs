use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct Tool {
    pub id: &'static str,
    pub name: &'static str,
    pub group: &'static str,
    /// Where a fresh open lands.
    pub url: &'static str,
    /// Route for ⌘N. Empty when the product has no distinct new-conversation URL.
    pub new_chat: &'static str,
}

const fn t(
    id: &'static str,
    name: &'static str,
    group: &'static str,
    url: &'static str,
    new_chat: &'static str,
) -> Tool {
    Tool { id, name, group, url, new_chat }
}

pub const TOOLS: &[Tool] = &[
    // ---- 国际 ----
    t("chatgpt", "ChatGPT", "global", "https://chatgpt.com/", "https://chatgpt.com/"),
    t("gemini", "Gemini", "global", "https://gemini.google.com/app", "https://gemini.google.com/app"),
    t("grok", "Grok", "global", "https://grok.com/", "https://grok.com/"),
    t("perplexity", "Perplexity", "global", "https://www.perplexity.ai/", "https://www.perplexity.ai/"),
    t("mistral", "Le Chat", "global", "https://chat.mistral.ai/chat", "https://chat.mistral.ai/chat"),
    t("poe", "Poe", "global", "https://poe.com/", "https://poe.com/"),
    t("huggingchat", "HuggingChat", "global", "https://huggingface.co/chat/", "https://huggingface.co/chat/"),
    t("openrouter", "OpenRouter", "global", "https://openrouter.ai/chat", "https://openrouter.ai/chat"),
    t("lmarena", "LMArena", "global", "https://lmarena.ai/", "https://lmarena.ai/"),
    t("notebooklm", "NotebookLM", "global", "https://notebooklm.google.com/", "https://notebooklm.google.com/"),
    t("aistudio", "AI Studio", "global", "https://aistudio.google.com/prompts/new_chat", "https://aistudio.google.com/prompts/new_chat"),
    t("piai", "Pi", "global", "https://pi.ai/talk", "https://pi.ai/talk"),
    t("metaai", "Meta AI", "global", "https://www.meta.ai/", "https://www.meta.ai/"),
    t("you", "You.com", "global", "https://you.com/", "https://you.com/"),
    // ---- 国内 ----
    t("deepseek", "DeepSeek", "cn", "https://chat.deepseek.com/", "https://chat.deepseek.com/"),
    t("kimi", "Kimi", "cn", "https://www.kimi.com/", "https://www.kimi.com/"),
    t("qwen", "Qwen", "cn", "https://chat.qwen.ai/", "https://chat.qwen.ai/"),
    t("doubao", "豆包", "cn", "https://www.doubao.com/chat/", "https://www.doubao.com/chat/"),
    t("yuanbao", "腾讯元宝", "cn", "https://yuanbao.tencent.com/chat", "https://yuanbao.tencent.com/chat"),
    t("tongyi", "通义", "cn", "https://www.tongyi.com/", "https://www.tongyi.com/"),
    t("wenxin", "文心一言", "cn", "https://yiyan.baidu.com/", "https://yiyan.baidu.com/"),
    t("zhipu", "智谱清言", "cn", "https://chatglm.cn/", "https://chatglm.cn/"),
    t("hailuo", "海螺 AI", "cn", "https://hailuoai.com/", "https://hailuoai.com/"),
    t("spark", "星火", "cn", "https://xinghuo.xfyun.cn/desk", "https://xinghuo.xfyun.cn/desk"),
    t("zai", "Z.ai", "cn", "https://zchat.z.ai/", "https://zchat.z.ai/"),
    // ---- 构建 ----
    t("v0", "v0", "build", "https://v0.app/", "https://v0.app/"),
    t("lovable", "Lovable", "build", "https://lovable.dev/", "https://lovable.dev/"),
    t("bolt", "Bolt", "build", "https://bolt.new/", "https://bolt.new/"),
    t("deepwiki", "DeepWiki", "build", "https://deepwiki.com/", "https://deepwiki.com/"),
];

pub fn get(id: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|t| t.id == id)
}
