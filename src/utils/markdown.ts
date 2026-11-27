import MarkdownIt from 'markdown-it'
import markdownItCjkFriendly from 'markdown-it-cjk-friendly'
import hljs from 'highlight.js/lib/common'

// 单例渲染器:
// - html:false  转义原始 HTML,防止 LLM 输出里的 <script>/<img onerror> 等被注入执行
// - linkify:true 自动识别纯文本链接
// - breaks:true  单个换行也渲染为 <br>(贴合聊天场景)
// 代码块走 highlight.js 高亮(仅注册常用语言,控制体积)。
const md: MarkdownIt = new MarkdownIt({
  html: false,
  linkify: true,
  breaks: true,
  highlight(str: string, lang: string): string {
    if (lang && hljs.getLanguage(lang)) {
      try {
        return `<pre class="hljs"><code>${hljs.highlight(str, { language: lang, ignoreIllegals: true }).value}</code></pre>`
      } catch {
        // 高亮异常则降级为纯转义,不中断
      }
    }
    return `<pre class="hljs"><code>${md.utils.escapeHtml(str)}</code></pre>`
  },
})

// CJK 友好:CommonMark 规定强调定界符内侧若是标点、外侧又不是空白/标点,则不算强调标记。
// 中文写作里 `**「标题」**`、`**内容。**` 这类写法极常见,裸用 markdown-it 会把 ** 当普通字符显示出来。
// 该插件重写定界符扫描规则,把强调标记**两侧的 CJK 字符视为与空白等效**,已通过 CommonMark 测试套件。
md.use(markdownItCjkFriendly)

/** 把 Markdown 源码渲染成 HTML 字符串(供 `v-html` 使用,已关闭原始 HTML 注入)。 */
export function renderMarkdown(src: string | null | undefined): string {
  return md.render(src ?? '')
}
