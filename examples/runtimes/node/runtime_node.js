const assert = require('node:assert/strict');
const http = require('node:http');
const os = require('node:os');

assert.equal(os.hostname(), 'node');
const server = http.createServer((request, response) => response.end('node ok'));
server.listen(8080, '127.0.0.1', () => {
  http.get('http://127.0.0.1:8080/', response => {
    assert.equal(response.statusCode, 200);
    let body = '';
    response.on('data', chunk => { body += chunk; });
    response.on('end', () => {
      assert.equal(body, 'node ok');
      server.close(() => console.log('Node HTTP passed'));
    });
  }).on('error', error => { throw error; });
});
