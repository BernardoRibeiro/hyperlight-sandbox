/** Bounded Bash-compatible toolbox. All commands and parsing run in this guest. */
import { getDirectories } from "wasi:filesystem/preopens@0.2.0";

const LIMITS = Object.freeze({ script: 65536, nodes: 10000, pipeline: 32, output: 1048576, expansion: 10000, depth: 64 });
const encoder = new TextEncoder();
const decoder = new TextDecoder();
let state = { cwd: "/work", env: Object.create(null), status: 0 };

class ShellError extends Error { constructor(message, status = 2) { super(message); this.status = status; } }
class Output {
  constructor() { this.stdout = ""; this.stderr = ""; this.truncated = false; }
  add(channel, text) {
    const room = LIMITS.output - this[channel].length;
    if (room <= 0) { this.truncated = true; return; }
    const chars = [...String(text)];
    const value = chars.slice(0, room).join("");
    this[channel] += value;
    if (value.length < text.length) this.truncated = true;
  }
}

function preopens() { return new Map(getDirectories().map(([descriptor, path]) => [path, descriptor])); }
function normalize(path) {
  const source = path.startsWith("/") ? path : `${state.cwd}/${path}`;
  const parts = [];
  for (const part of source.split("/")) {
    if (!part || part === ".") continue;
    if (part === "..") { if (!parts.length) throw new ShellError("path escapes a mount", 1); parts.pop(); }
    else if (part.includes("\\") || part.includes("\0")) throw new ShellError("invalid path", 1);
    else parts.push(part);
  }
  return `/${parts.join("/")}`;
}
function resolve(path) {
  const absolute = normalize(path); const pieces = absolute.slice(1).split("/");
  const mount = `/${pieces.shift()}`; const descriptor = preopens().get(mount);
  if (!descriptor) throw new ShellError(`${path}: unavailable mount`, 1);
  return { absolute, mount, name: pieces.join("/"), descriptor };
}
function open(path, flags = {}, descriptorFlags = { read: true }) {
  const item = resolve(path); if (!item.name) return item.descriptor;
  return item.descriptor.openAt({}, item.name, flags, descriptorFlags);
}
function readBytes(path) { const fd = open(path); const stat = fd.stat(); return fd.readViaStream(0n).blockingRead(stat.size); }
function readText(path) { return decoder.decode(readBytes(path)); }
function writeText(path, data, append = false) {
  const fd = open(path, { create: true, truncate: !append }, { read: true, write: true });
  const offset = append ? fd.stat().size : 0n;
  fd.writeViaStream(offset).blockingWriteAndFlush(encoder.encode(data));
}
function parent(path) { const absolute = normalize(path); const slash = absolute.lastIndexOf("/"); return [absolute.slice(0, slash) || "/", absolute.slice(slash + 1)]; }
function entries(path) {
  const fd = open(path); const stream = fd.readDirectory(); const result = [];
  while (true) { const entry = stream.readDirectoryEntry(); if (entry === undefined) break; result.push(entry); if (result.length > LIMITS.expansion) throw new ShellError("directory entry limit exceeded", 1); }
  return result;
}
function stat(path) { return open(path).stat(); }

function tokenize(script) {
  if (encoder.encode(script).length > LIMITS.script) throw new ShellError("script exceeds 64 KiB");
  const tokens = []; let fragments = []; let text = ""; let quote = null; let quoted = false;
  const flushText = () => { if (text || quoted) { fragments.push({ text, quote }); text = ""; quoted = false; } };
  const flushWord = () => { flushText(); if (fragments.length) { tokens.push({ type: "word", fragments }); fragments = []; } };
  for (let i = 0; i < script.length; i++) {
    const c = script[i];
    if (quote) {
      if (c === quote) { flushText(); quote = null; quoted = true; }
      else if (c === "\\" && quote === '"' && i + 1 < script.length) text += script[++i];
      else text += c;
      continue;
    }
    if (c === "'" || c === '"') { flushText(); quote = c; quoted = true; continue; }
    if (c === "\\") { if (++i >= script.length) throw new ShellError("trailing escape"); text += script[i]; continue; }
    if (/\s/.test(c)) { flushWord(); if (c === "\n") tokens.push({ type: ";" }); continue; }
    const two = script.slice(i, i + 2);
    if (["&&", "||", ">>"].includes(two)) { flushWord(); tokens.push({ type: two }); i++; continue; }
    if ([";", "|", "<", ">"].includes(c)) { flushWord(); tokens.push({ type: c }); continue; }
    text += c;
  }
  if (quote) throw new ShellError("unterminated quote"); flushWord();
  return tokens;
}
function parse(script) {
  const tokens = tokenize(script); let index = 0; let nodes = 0;
  const command = () => {
    const words = [], redirects = [];
    while (index < tokens.length && ![";", "&&", "||", "|"].includes(tokens[index].type)) {
      const token = tokens[index++];
      if (["<", ">", ">>"].includes(token.type)) { const target = tokens[index++]; if (!target || target.type !== "word") throw new ShellError("redirection requires a path"); redirects.push({ op: token.type, target }); }
      else if (token.type === "word") words.push(token); else throw new ShellError(`unexpected ${token.type}`);
    }
    if (!words.length) throw new ShellError("expected command"); if (++nodes > LIMITS.nodes) throw new ShellError("AST node limit exceeded");
    return { words, redirects };
  };
  const pipelines = [];
  while (index < tokens.length) {
    while (tokens[index]?.type === ";") index++; if (index >= tokens.length) break;
    const stages = [command()]; while (tokens[index]?.type === "|") { index++; stages.push(command()); if (stages.length > LIMITS.pipeline) throw new ShellError("pipeline limit exceeded"); }
    const next = tokens[index]?.type; pipelines.push({ join: pipelines.length ? pipelines[pipelines.length - 1].next : ";", stages, next: ["&&", "||", ";"].includes(next) ? next : ";" });
    if (next) index++;
  }
  return pipelines;
}
function expandWord(word) {
  let value = "";
  for (const fragment of word.fragments) {
    if (fragment.quote === "'") value += fragment.text;
    else value += fragment.text.replace(/\$\?|\$\{([A-Za-z_][A-Za-z0-9_]*)\}|\$([A-Za-z_][A-Za-z0-9_]*)/g, (match, braced, plain) => match === "$?" ? String(state.status) : (state.env[braced || plain] ?? ""));
  }
  return value;
}
function glob(word) {
  if (!/[?*]/.test(word)) return [word];
  const [directory, pattern] = parent(word); const expression = new RegExp(`^${pattern.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*").replace(/\?/g, ".")}$`);
  const matches = entries(directory).map(e => e.name).filter(name => expression.test(name)).sort().map(name => normalize(`${directory}/${name}`));
  return matches.length ? matches : [word];
}
function words(command) { return command.words.flatMap(word => glob(expandWord(word))); }
function lines(input) { return input.split(/(?<=\n)/); }
function parseCount(args, fallback = 10) { const i = args.indexOf("-n"); return i >= 0 ? Number(args[i + 1]) : fallback; }

const commands = {
  true: () => ({ status: 0 }), false: () => ({ status: 1 }),
  echo: args => ({ status: 0, stdout: `${args.join(" ")}\n` }),
  printf: args => ({ status: 0, stdout: (args.shift() || "").replace(/%s/g, () => args.shift() ?? "").replace(/\\n/g, "\n") }),
  pwd: () => ({ status: 0, stdout: `${state.cwd}\n` }),
  cd: args => { const path = normalize(args[0] || "/work"); stat(path); state.cwd = path; return { status: 0 }; },
  export: args => { for (const value of args) { const [key, ...rest] = value.split("="); if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) throw new ShellError("export: invalid name", 1); state.env[key] = rest.join("="); } return { status: 0 }; },
  unset: args => { for (const key of args) delete state.env[key]; return { status: 0 }; },
  cat: (args, stdin) => ({ status: 0, stdout: args.length ? args.map(readText).join("") : stdin }),
  head: (args, stdin) => { const files = args.filter((a, i) => a !== "-n" && args[i - 1] !== "-n"); return { status: 0, stdout: lines(files.length ? files.map(readText).join("") : stdin).slice(0, parseCount(args)).join("") }; },
  tail: (args, stdin) => { const files = args.filter((a, i) => a !== "-n" && args[i - 1] !== "-n"); return { status: 0, stdout: lines(files.length ? files.map(readText).join("") : stdin).slice(-parseCount(args)).join("") }; },
  wc: (args, stdin) => { const text = args.filter(a => !a.startsWith("-")).map(readText).join("") || stdin; const count = args.includes("-l") ? text.split("\n").length - 1 : args.includes("-w") ? (text.trim().match(/\S+/g) || []).length : encoder.encode(text).length; return { status: 0, stdout: `${count}\n` }; },
  ls: args => ({ status: 0, stdout: entries(args.find(a => !a.startsWith("-")) || state.cwd).map(e => e.name).sort().join("\n") + "\n" }),
  mkdir: args => { for (const path of args.filter(a => !a.startsWith("-"))) { const [dir, name] = parent(path); open(dir).createDirectoryAt(name); } return { status: 0 }; },
  touch: args => { for (const path of args) open(path, { create: true }, { read: true, write: true }); return { status: 0 }; },
  rm: args => { for (const path of args.filter(a => !a.startsWith("-"))) { const [dir, name] = parent(path); open(dir).unlinkFileAt(name); } return { status: 0 }; },
  mv: args => { if (args.length !== 2) throw new ShellError("mv: expected source and destination", 1); const [sd, sn] = parent(args[0]), [dd, dn] = parent(args[1]); open(sd).renameAt(sn, open(dd), dn); return { status: 0 }; },
  cp: args => { if (args.length !== 2) throw new ShellError("cp: expected source and destination", 1); writeText(args[1], readText(args[0])); return { status: 0 }; },
  grep: (args, stdin) => { const recursive = args.includes("-R") || args.includes("-r"); const clean = args.filter(a => !a.startsWith("-")); const pattern = clean.shift(); if (!pattern) throw new ShellError("grep: missing pattern", 2); const output = []; const scan = (path, depth = 0) => { if (depth > LIMITS.depth) throw new ShellError("grep: traversal depth exceeded", 1); try { const text = readText(path); for (const line of text.split("\n")) if (line.includes(pattern)) output.push(`${recursive ? `${path}:` : ""}${line}`); } catch (error) { if (!recursive) throw error; for (const entry of entries(path)) scan(`${path}/${entry.name}`, depth + 1); } }; if (clean.length) clean.forEach(path => scan(path)); else for (const line of stdin.split("\n")) if (line.includes(pattern)) output.push(line); return { status: output.length ? 0 : 1, stdout: output.length ? `${output.join("\n")}\n` : "" }; },
  find: args => { const root = args[0] && !args[0].startsWith("-") ? args[0] : state.cwd; const nameIndex = args.indexOf("-name"), pattern = nameIndex >= 0 ? args[nameIndex + 1] : "*"; const expression = new RegExp(`^${pattern.replace(/\./g, "\\.").replace(/\*/g, ".*").replace(/\?/g, ".")}$`); const found = []; const walk = (path, depth = 0) => { if (depth > LIMITS.depth) throw new ShellError("find: traversal depth exceeded", 1); const base = path.split("/").pop(); if (depth === 0 || expression.test(base)) found.push(path); try { for (const entry of entries(path)) walk(`${path}/${entry.name}`, depth + 1); } catch {} }; walk(normalize(root)); return { status: 0, stdout: `${found.join("\n")}\n` }; },
  sort: (args, stdin) => { const text = args.length ? args.map(readText).join("") : stdin; return { status: 0, stdout: text.split("\n").filter((line, index, all) => line || index < all.length - 1).sort().join("\n") + (text ? "\n" : "") }; },
  test: args => { let yes; if (args[0] === "-e") { try { stat(args[1]); yes = true; } catch { yes = false; } } else if (args[0] === "-s") { try { yes = stat(args[1]).size > 0n; } catch { yes = false; } } else if (args.length === 3 && args[1] === "=") yes = args[0] === args[2]; else yes = Boolean(args[0]); return { status: yes ? 0 : 1 }; },
};
commands["["] = args => { if (args.pop() !== "]") throw new ShellError("[: missing ]", 2); return commands.test(args); };
commands.sed = args => { const expression = args.shift(); const match = /^s(.)(.*?)\1(.*?)\1(g?)$/.exec(expression || ""); if (!match) throw new ShellError("sed: only s/old/new/[g] is supported", 2); const input = args.length ? args.map(readText).join("") : ""; return { status: 0, stdout: match[4] ? input.split(match[2]).join(match[3]) : input.replace(match[2], match[3]) }; };

function runCommand(command, stdin) {
  const argv = words(command); const name = argv.shift();
  for (const redirect of command.redirects) if (redirect.op === "<") stdin = readText(expandWord(redirect.target));
  let assignment = /^([A-Za-z_][A-Za-z0-9_]*)=(.*)$/.exec(name); if (assignment) { state.env[assignment[1]] = assignment[2]; return { status: 0, stdout: "", stderr: "" }; }
  const implementation = commands[name]; if (!implementation) return { status: 127, stdout: "", stderr: `${name}: command not found\ntoolbox:unsupported-command:${JSON.stringify({ command: name })}\n` };
  let result; try { result = implementation(argv, stdin) || { status: 0 }; } catch (error) { result = { status: error.status || 1, stderr: `${name}: ${error.message || error}\n` }; }
  result.stdout ||= ""; result.stderr ||= "";
  for (const redirect of command.redirects) if (redirect.op !== "<") { writeText(expandWord(redirect.target), result.stdout, redirect.op === ">>"); result.stdout = ""; }
  return result;
}
function execute(script) {
  const output = new Output(); let previous = 0;
  for (const pipeline of parse(script)) {
    if ((pipeline.join === "&&" && previous !== 0) || (pipeline.join === "||" && previous === 0)) continue;
    let stdin = "", stderr = "";
    for (const stage of pipeline.stages) {
      const result = runCommand(stage, stdin);
      const combinedError = `${stderr}${result.stderr}`;
      stdin = [...result.stdout].slice(0, LIMITS.output).join("");
      stderr = [...combinedError].slice(0, LIMITS.output).join("");
      if (stdin.length < result.stdout.length || stderr.length < combinedError.length) output.truncated = true;
      previous = result.status; state.status = previous;
    }
    output.add("stdout", stdin); output.add("stderr", stderr);
  }
  if (output.truncated) {
    const marker = "[toolbox output truncated]\n";
    output.stderr = `${output.stderr.slice(0, Math.max(0, LIMITS.output - marker.length))}${marker}`;
  }
  return { stdout: output.stdout, stderr: output.stderr, exitCode: previous };
}

export const executor = { async run(script) { try { return execute(script); } catch (error) { return { stdout: "", stderr: `toolbox: ${error.message || error}\n`, exitCode: error.status || 2 }; } } };
