// A document with things to do → its text, for the panel's «Планы» and the to-do window
// (/todo). The speaker makes tasks of the text (planner::parse_document); this only reads the
// file: .txt / .md / .csv / .ics / .json as they are, .docx by unpacking it here (a .docx is a
// ZIP with word/document.xml inside; DecompressionStream inflates it, no library needed).
'use strict';

// Paragraphs and table rows of word/document.xml as lines: a table row is «a | b | c» (the
// way a day plan is written), a numbered or bulleted paragraph starts with «- ».
function docxLines(xml) {
  const doc = new DOMParser().parseFromString(xml, 'application/xml');
  const body = doc.getElementsByTagName('w:body')[0];
  if (!body) throw new Error('в файле нет текста документа');
  const text = el => [...el.getElementsByTagName('*')]
    .filter(n => n.nodeName === 'w:t' || n.nodeName === 'w:tab' || n.nodeName === 'w:br')
    .map(n => (n.nodeName === 'w:t' ? n.textContent : n.nodeName === 'w:tab' ? '\t' : ' '))
    .join('');
  const lines = [];
  for (const el of body.children) {
    if (el.nodeName === 'w:p') {
      const t = text(el).trim();
      lines.push(t && el.getElementsByTagName('w:numPr').length ? '- ' + t : t);
    } else if (el.nodeName === 'w:tbl') {
      for (const tr of el.getElementsByTagName('w:tr')) {
        lines.push([...tr.getElementsByTagName('w:tc')].map(tc => text(tc).replace(/\s+/g, ' ').trim()).join(' | '));
      }
    }
  }
  return lines.join('\n');
}

async function docxText(file) {
  const buf = new Uint8Array(await file.arrayBuffer());
  const dv = new DataView(buf.buffer);
  // The ZIP's end record (signature 0x06054b50) is at the end, before an optional comment.
  let end = -1;
  for (let i = buf.length - 22; i >= Math.max(0, buf.length - 66000); i--) {
    if (dv.getUint32(i, true) === 0x06054b50) { end = i; break; }
  }
  if (end < 0) throw new Error('это не документ Word (.docx)');
  const count = dv.getUint16(end + 10, true);
  let p = dv.getUint32(end + 16, true);
  for (let n = 0; n < count && p + 46 <= buf.length; n++) {
    if (dv.getUint32(p, true) !== 0x02014b50) break;
    const method = dv.getUint16(p + 10, true), size = dv.getUint32(p + 20, true);
    const nameLen = dv.getUint16(p + 28, true), extraLen = dv.getUint16(p + 30, true), commentLen = dv.getUint16(p + 32, true);
    const local = dv.getUint32(p + 42, true);
    const name = new TextDecoder().decode(buf.subarray(p + 46, p + 46 + nameLen));
    if (name === 'word/document.xml') {
      const start = local + 30 + dv.getUint16(local + 26, true) + dv.getUint16(local + 28, true);
      const data = buf.subarray(start, start + size);
      if (method === 0) return docxLines(new TextDecoder().decode(data));
      if (method !== 8 || !window.DecompressionStream) throw new Error('этот браузер не умеет читать .docx — сохраните документ как текст (.txt)');
      const xml = await new Response(new Blob([data]).stream().pipeThrough(new DecompressionStream('deflate-raw'))).text();
      return docxLines(xml);
    }
    p += 46 + nameLen + extraLen + commentLen;
  }
  throw new Error('в файле нет текста документа');
}

// The file's text; throws an Error with a Russian message when it cannot be read.
async function readDocFile(file) {
  if (file.size > 2 * 1024 * 1024) throw new Error('файл больше 2 МБ — это точно список дел?');
  const name = (file.name || '').toLowerCase();
  if (name.endsWith('.docx')) return fits(await docxText(file));
  if (/\.(doc|pdf|xlsx?|pages|rtf|odt)$/.test(name)) {
    throw new Error('такой файл прочитать не могу. Подойдут .txt, .md, .docx, .csv, .ics, .json — или вставьте текст');
  }
  // Notepad and Excel on a Russian Windows save «ANSI» (Windows-1251) or UTF-16, not UTF-8.
  const buf = new Uint8Array(await file.arrayBuffer());
  let text;
  if (buf[0] === 0xFF && buf[1] === 0xFE) text = new TextDecoder('utf-16le').decode(buf);
  else if (buf[0] === 0xFE && buf[1] === 0xFF) text = new TextDecoder('utf-16be').decode(buf);
  else {
    text = new TextDecoder('utf-8').decode(buf);
    if (text.includes('\ufffd') && !/[\u0000-\u0008]/.test(text.slice(0, 4000))) text = new TextDecoder('windows-1251').decode(buf);
  }
  // A binary file read as text is full of replacement and control characters.
  if (/[\u0000-\u0008\ufffd]/.test(text.slice(0, 4000))) throw new Error('это не текстовый файл. Подойдут .txt, .md, .docx, .csv, .ics, .json');
  return fits(text);
}

// The speaker takes a command of up to 64 KB: a longer document is refused here, in words.
function fits(text) {
  if (new Blob([JSON.stringify(text)]).size > 60 * 1024) {
    throw new Error('в документе слишком много текста для колонки (больше ~60 КБ) — разделите его на части или оставьте только будущие дела');
  }
  return text;
}
