export const context7AskEndpoint =
  "https://context7.com/api/v2/ask?siteKey=ask_b8918f0414955bb91bfa1a89";

export const context7VerificationTag = `<meta name="context7-site-verification" content="${new URL(context7AskEndpoint).searchParams.get("siteKey")}">`;

const pageIndex = "Page index: https://tako.sh/llms.txt. Docs and blog posts also have .md URLs.";
const searchNote = `This site has a search endpoint: ${context7AskEndpoint}&query=<URL-encoded question>. It returns documentation that answers the question, with a source link for each part. No API key is needed. If nothing matches, it says so.`;

export const context7AskNote = `<blockquote data-context7-ask="true" aria-hidden="true" style="position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);white-space:nowrap;border:0">
<h2>Search</h2>
<p>${pageIndex}</p>
<p>${searchNote.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;")}</p>
</blockquote>`;

export const context7SearchSection = `## Search\n\n${searchNote}\n`;
export const context7MarkdownNote = `## Search\n\n${pageIndex}\n\n${searchNote}\n\n`;
export const context7SearchLink = `<${context7AskEndpoint}>; rel="search"; type="text/plain"; title="Search tako.sh"`;
