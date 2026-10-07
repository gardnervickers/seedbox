// Execute the actual github-script bodies with a fake API. Run with Node.js;
// Actions provides Node, while the portable project shell does not require it.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const workflow = fs.readFileSync(path.join(__dirname, '../../.github/workflows/host-check.yml'), 'utf8');
const scripts = [...workflow.matchAll(/^          script: \|\n((?:            .*\n|\n)+)/gm)]
  .map(match => match[1].replace(/^            /gm, ''));
assert.equal(scripts.length, 3);
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
const gate = new AsyncFunction('context', 'github', 'core', scripts[0]);
const report = new AsyncFunction('context', 'github', 'process', 'require', scripts[2]);
const sha = 'a'.repeat(40);

async function request({event = 'pull_request_target', action = 'labeled', owner = 'gardnervickers',
    head = sha, state = 'open', repo = 'gardnervickers/seedbox', fork = false,
    label = 'needs-host', body = '/host-check', labelled = true, removeError, files = []} = {}) {
  const calls = [], outputs = {};
  const context = {eventName: event, repo: {owner: 'gardnervickers', repo: 'seedbox'}, payload: {
    action, label: {name: label}, sender: {login: owner},
    pull_request: {number: 112, head: {sha}}, issue: {number: 112, pull_request: {}},
    comment: {user: {login: owner}, body},
  }};
  const github = {rest: {
    pulls: {
      get: async () => {
        calls.push('get');
        return {data: {state, head: {sha: head, repo: {full_name: repo, fork}},
          labels: labelled ? [{name: 'needs-host'}] : []}};
      },
      listFiles: 'files',
    },
    issues: {removeLabel: async args => {
      assert.equal(args.name, 'needs-host');
      assert.equal(args.issue_number, 112);
      calls.push('remove');
      if (removeError) throw {status: removeError};
    }},
  }, paginate: async () => files};
  const core = {setOutput: (key, value) => { calls.push(`output:${key}`); outputs[key] = value; }};
  await gate(context, github, core);
  return {calls, outputs};
}

(async () => {
  for (const event of ['label', 'comment']) {
    const result = await request(event === 'comment' ? {event: 'issue_comment', action: 'created'} : {});
    assert.equal(result.outputs.allowed, 'true');
    assert.equal(result.outputs.sha, sha);
    assert.equal(result.outputs.mode, 'dev');
    assert(result.calls.indexOf('remove') < result.calls.indexOf('output:allowed'));
  }
  for (const action of ['synchronize', 'closed']) {
    const result = await request({action});
    assert.deepEqual(result, {calls: [], outputs: {}});
  }
  for (const invalid of [{owner: 'other'}, {label: 'other'}, {head: 'b'.repeat(40)},
      {state: 'closed'}, {repo: 'other/seedbox'}, {fork: true}, {labelled: false},
      {event: 'issue_comment', action: 'created', owner: 'other'},
      {event: 'issue_comment', action: 'created', body: '/host-check extra'}]) {
    const result = await request(invalid);
    assert.deepEqual(result.outputs, {});
    assert(!result.calls.includes('remove'));
  }
  // A command is valid without a label; a disappeared label request is not.
  assert.equal((await request({event: 'issue_comment', action: 'created', labelled: false,
    removeError: 404})).outputs.allowed, 'true');
  for (const removeError of [403, 404, 500]) {
    await assert.rejects(request({removeError}), error => error.status === removeError);
  }
  assert.equal((await request({files: [{filename: 'crates/seedbox/src/admission.rs'}]})).outputs.mode, 'pinned');
  await assert.rejects(request({files: Array(3000).fill({filename: 'file'})}), /too large/);

  // Reporting never consumes a label that was added after this job started.
  for (const head of [sha, 'b'.repeat(40)]) {
    let comments = 0;
    await report({repo: {}}, {rest: {
      pulls: {get: async () => ({data: {state: 'open', head: {sha: head,
        repo: {full_name: 'gardnervickers/seedbox'}}}})},
      issues: {createComment: async () => { comments++; }, removeLabel: async () => assert.fail('late removal')},
    }}, {env: {NUMBER: '112', SHA: sha}}, () => ({readFileSync: () => 'summary'}));
    assert.equal(comments, head === sha ? 1 : 0);
  }
  const host = workflow.split('\n  host:\n')[1].split('\n  report:\n')[0];
  assert.match(host, /fetch-depth: 0/);
  assert.match(host, /persist-credentials: false/);
  assert.match(host, /git cat-file -e "\$SHA\^\{commit\}"/);
  assert(!host.includes('git fetch'));
  assert(!host.includes('issues: write'));
  // Push/closure cancellation stays active, but the gate admits only requests.
  assert.match(workflow.split('\njobs:\n')[0], /action == 'synchronize'/);
  assert(!workflow.split('\n  gate:\n')[1].split('runs-on:')[0].includes("action == 'synchronize'"));
  console.log('host workflow controls passed');
})().catch(error => { console.error(error); process.exitCode = 1; });
