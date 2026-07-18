// One-shot import of historical daily totals from wakatime.com into the
// wakatime_days table, so the dashboard has history from day one.
// Uses summaries (available further back than raw heartbeats on free plans).
import { openDb, upsertWakatimeDay } from './db.js';

const API = 'https://api.wakatime.com/api/v1';

function fmt(d) { return d.toISOString().slice(0, 10); }

async function fetchRetry(url, opts, tries = 4) {
  for (let i = 1; ; i++) {
    try {
      return await fetch(url, opts);
    } catch (err) {
      if (i >= tries) throw err;
      console.log(`[stackhour] fetch failed (${err.cause?.code || err.message}), retry ${i}/${tries - 1}`);
      await new Promise((r) => setTimeout(r, 1500 * i));
    }
  }
}

export async function importWakatime(cfg, { days = 365 } = {}) {
  const key = cfg.wakatime.apiKey || process.env.WAKATIME_API_KEY;
  if (!key) throw new Error('no wakatime.apiKey in config and no WAKATIME_API_KEY set');
  const auth = 'Basic ' + Buffer.from(key).toString('base64');
  const db = openDb(cfg.server.db);

  const end = new Date();
  let imported = 0;
  // summaries endpoint allows ranges; chunk by 30 days to be polite
  for (let offset = 0; offset < days; offset += 30) {
    const chunkEnd = new Date(end.getTime() - offset * 86400_000);
    const chunkStart = new Date(end.getTime() - Math.min(offset + 29, days - 1) * 86400_000);
    const url = `${API}/users/current/summaries?start=${fmt(chunkStart)}&end=${fmt(chunkEnd)}`;
    const res = await fetchRetry(url, { headers: { authorization: auth } });
    if (res.status === 402) {
      console.log(`[stackhour] wakatime: range ${fmt(chunkStart)}..${fmt(chunkEnd)} needs a paid plan; stopping (imported what was available)`);
      break;
    }
    if (!res.ok) throw new Error(`wakatime API ${res.status}: ${await res.text()}`);
    const body = await res.json();
    for (const day of body.data || []) {
      const date = day.range?.date;
      if (!date) continue;
      for (const p of day.projects || []) {
        upsertWakatimeDay(db, date, p.name, p.total_seconds || 0);
        imported++;
      }
    }
    console.log(`[stackhour] imported ${fmt(chunkStart)}..${fmt(chunkEnd)}`);
  }
  console.log(`[stackhour] done: ${imported} day-project rows in wakatime_days`);
}
