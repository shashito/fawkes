// A tiny app for trying Fawkes. It serves ./VERSION on APP_PORT and records a row in its SQLite storage
// ($APP_HOME/app.sqlite, WAL mode) on every start, so a rollback of the storage is visible.
//
// GET /crash makes it exit with status 7. ./MODE picks what it does after listening:
//   confirm (default)  calls `fawkes confirm` when it runs live; during a test it only serves
//   never              never confirms, so the confirm deadline rolls it back
//   confirm-always     runs a bare `fawkes confirm` in every phase, its test included, as an app that neither names
//                      itself nor looks at FAWKES_PHASE would (the answer goes to ./confirm.json)
//   crash              exits with status 3 straight away, also in a test; with $CRASH_WHILE set, only while the file it
//                      names exists (a marker that lets a test fail and then pass, as the code of a generation is fixed)
//   crash-live         like crash, but only when running live (so it passes its test)
//   crash-confirm      when live: leaves a process that ignores SIGTERM in its group, has `fawkes confirm` sent from
//                      outside the group 0.3 s later (the answer goes to $APP_HOME/confirm.json), and exits with
//                      status 3: a confirm racing the crash of the generation it confirms
//   unhealthy-live     when live, answers every request with 500 and never confirms (it passes its test)
//   slow-stop          like confirm, but takes 1.5 s to exit on SIGTERM
//   slow-stop-never    like never, but takes 1.5 s to exit on SIGTERM
//   slow-test          during a test, starts listening only after 6 s; live it confirms
const { DatabaseSync } = require('node:sqlite');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const { spawn, spawnSync } = require('node:child_process');

const read = (file, fallback) => {
  try { return fs.readFileSync(path.join(__dirname, file), 'utf8').trim(); } catch { return fallback; }
};
const version = read('VERSION', 'dev');
const mode = read('MODE', 'confirm');
const phase = process.env.FAWKES_PHASE;

const crashes = !process.env.CRASH_WHILE || fs.existsSync(process.env.CRASH_WHILE);
if ((mode === 'crash' && crashes) || (mode === 'crash-live' && phase === 'live')) {
  console.error(`${version}: crashing on start`);
  process.exit(3);
}

if (mode === 'crash-confirm' && phase === 'live') {
  spawn('/bin/sh', ['-c', 'trap "" TERM; sleep 5'], { stdio: 'ignore' });
  const confirm = 'sleep 0.3; "$FAWKES_BIN" --json confirm "$FAWKES_GENERATION" > "$APP_HOME/confirm.json"';
  spawn('/bin/sh', ['-c', confirm], { detached: true, stdio: 'ignore' }).unref();
  process.exit(3);
}

const db = new DatabaseSync(path.join(process.env.APP_HOME, 'app.sqlite'));
db.exec('PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS starts (version TEXT, phase TEXT)');
db.prepare('INSERT INTO starts VALUES (?, ?)').run(version, phase);

const server = http.createServer((req, res) => {
  if (req.url === '/crash') process.exit(7); // for trying out crash restarts
  if (mode === 'unhealthy-live' && phase === 'live') res.statusCode = 500;
  res.end(version);
});
const listen = () => server.listen(Number(process.env.APP_PORT), '127.0.0.1', () => {
  console.log(`${version} listening on ${process.env.APP_PORT} (${phase})`);
  if (['confirm', 'slow-stop', 'slow-test'].includes(mode) && phase === 'live') {
    // FAWKES_BIN, FAWKES_SOCKET and FAWKES_GENERATION are in our environment. Nothing is pending (and the
    // confirm is refused) when this generation is merely restarted; that is fine.
    spawnSync(process.env.FAWKES_BIN, ['confirm', process.env.FAWKES_GENERATION], { stdio: 'inherit' });
  }
  if (mode === 'confirm-always') {
    spawnSync('/bin/sh', ['-c', '"$FAWKES_BIN" --json confirm > confirm.json'], { cwd: __dirname, stdio: 'inherit' });
  }
});
if (mode === 'slow-test' && phase === 'test') setTimeout(listen, 6000);
else listen();

process.on('SIGTERM', () => {
  setTimeout(() => {
    server.close();
    db.close();
    process.exit(0);
  }, mode.startsWith('slow-stop') ? 1500 : 0);
});
