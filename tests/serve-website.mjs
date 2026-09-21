import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname, sep } from 'node:path';
const root = resolve('website/dist');
const types = { '.html': 'text/html; charset=utf-8', '.css': 'text/css', '.js': 'text/javascript', '.json': 'application/json', '.png': 'image/png', '.woff2': 'font/woff2' };
createServer(async (request, response) => {
  try {
    const url = new URL(request.url, 'http://localhost');
    // The /preview/ prefix deliberately tests GitHub project Pages deployment.
    let path = decodeURIComponent(url.pathname).replace(/^\/preview\//, '/');
    if (path.endsWith('/')) path += 'index.html';
    const file = resolve(root, '.' + path);
    // `sep`, not '/': on Windows the resolved path uses backslashes, so a
    // forward-slash prefix would reject every file and answer 404.
    if (!file.startsWith(root + sep)) throw Error('Invalid path');
    response.setHeader('Content-Type', types[extname(file)] || 'application/octet-stream');
    response.end(await readFile(file));
  } catch { response.writeHead(404); response.end('Not found'); }
}).listen(15175, '127.0.0.1');
