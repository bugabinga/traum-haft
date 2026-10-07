// The auto-triage routine's job, as the platform agent: read the visitor
// report, ship a fix, close the issue; builder and reporter get emails.
import { McpClient } from "./mcp-client.mjs";

let pass = 0, fail = 0;
async function check(name, fn) {
  try { await fn(); console.log(`PASS  ${name}`); pass++; }
  catch (e) { console.log(`FAIL  ${name}: ${String(e.message ?? e).split("\n")[0]}`); fail++; }
}
function assert(c, m) { if (!c) throw new Error(m); }
const seen = async () => (await fetch("http://127.0.0.1:39400/_seen")).json();

const triage = await new McpClient("https://mcp.apps.isp-insoft.de", "triage@isp-insoft.de").connect();
let number;
await check("triage reads the report (marked untrusted)", async () => {
  const r = await triage.tool("list_issues", { app: "board" });
  assert(r.ok && r.text.startsWith("Untrusted content"), r.text.slice(0, 100));
  const list = JSON.parse(r.text.slice(r.text.indexOf("\n") + 1));
  const report = list.find((i) => i.labels.includes("visitor-report") && i.body.includes("nicht sortiert"));
  assert(report, JSON.stringify(list));
  number = report.number;
});
await check("triage ships the fix; issue closed; reporter and builder emailed", async () => {
  const d = await triage.deploy({ app: "board", message: "Notizen nach Datum sortiert", fixes_issue: number });
  assert(d.ok && d.text.includes("closed"), d.text);
  const s = await seen();
  assert(s.issues[number - 1].state === "closed", "issue still open");
  assert(s.comments.some((c) => c.number === number && c.body.includes("Fixed in version")), "no comment");
  const mails = s.mails.map((m) => [/^To: (.*)$/m.exec(m)?.[1], /^Subject: (.*)$/m.exec(m)?.[1]]);
  assert(mails.some(([to, sub]) => to === "bob@isp-insoft.de" && /behoben/.test(sub ?? "")), JSON.stringify(mails));
  assert(mails.some(([to, sub]) => to === "alice@isp-insoft.de" && /Korrektur live/.test(sub ?? "")), JSON.stringify(mails));
});
console.log(`== triage: ${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
