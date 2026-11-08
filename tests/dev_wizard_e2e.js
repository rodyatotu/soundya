#!/usr/bin/env node
// The installer's developer build: the wizard with the feedback mode in Chromium (Playwright);
// notes kept by the installer (its port changes every start), «Сохранить файлом» into ~/Downloads.
//   (cd installer && cargo build --features dev) && node tests/dev_wizard_e2e.js
'use strict';
const { chromium } = require('playwright');
const { spawn } = require('child_process');
const fs=require('fs'),os=require('os'),path=require('path');
(async()=>{
 const home=fs.mkdtempSync(path.join(os.tmpdir(),'wh-')); fs.mkdirSync(home+'/Downloads');
 const inst=spawn('installer/target/debug/soundya-installer',[],{env:Object.assign({},process.env,{SOUNDYA_INSTALLER_NO_BROWSER:'1',HOME:home})});
 const url=await new Promise(ok=>{let b='';inst.stdout.on('data',d=>{b+=d;const m=b.match(/http:\/\/127\.0\.0\.1:\d+\//);if(m)ok(m[0]);});inst.stderr.on('data',d=>{b+=d;const m=b.match(/http:\/\/127\.0\.0\.1:\d+\//);if(m)ok(m[0]);});});
 let fails=0;const check=(n,ok,x)=>{console.log((ok?'  ok   ':'  FAIL ')+n+(ok?'':' — '+x));fails+=ok?0:1;};
 const b=await chromium.launch();const p=await b.newPage({viewport:{width:1000,height:700}});await p.goto(url+'?app=1');
 await p.waitForFunction(()=>window.__sdFeedback);
 const info=await (await p.request.get(url+'api/info')).json();
 check('version +dev', /\+dev$/.test(info.version), info.version);
 const r=await p.evaluate(()=>{const h=[...document.querySelectorAll('h1,h2,button')].find(x=>{const b=x.getBoundingClientRect();return b.width>20&&b.top>0&&b.bottom<innerHeight&&!x.closest('[data-sd]')});const b=h.getBoundingClientRect();return {x:b.left+b.width/2,y:b.top+b.height/2}});
 await p.keyboard.press('Alt+KeyF');await p.mouse.move(r.x,r.y);await p.mouse.click(r.x,r.y);
 await p.locator('[data-sd] >> css=.kinds button[data-k="smaller"]').click();
 await p.locator('[data-sd] >> css=.card .text').fill('мелочь');
 await p.locator('[data-sd] >> css=.card .ok').click();
 await p.waitForFunction(()=>window.__sdFeedback.notes().length===1,null,{timeout:15000});
 await p.waitForTimeout(500);
 const stored=fs.readFileSync(home+'/.config/soundya-installer/dev-notes.json','utf8');
 check('notes kept by the installer (port changes)', /мелочь/.test(stored) && /data:image\/jpeg/.test(stored), stored.slice(0,80));
 await p.reload(); await p.waitForFunction(()=>window.__sdFeedback&&window.__sdFeedback.notes().length===1,null,{timeout:5000}).catch(()=>{});
 check('after reload', (await p.evaluate(()=>window.__sdFeedback.notes().length))===1);
 await p.locator('[data-sd] >> css=.fab').click();
 await p.locator('[data-sd] >> css=.file').click(); await p.waitForTimeout(1000);
 const dirs=fs.readdirSync(home+'/Downloads');
 const files=dirs.length?fs.readdirSync(home+'/Downloads/'+dirs[0]):[];
 check('saved into ~/Downloads', files.includes('prompt.md')&&files.includes('shot-1.jpg'), JSON.stringify([dirs,files]));
 const md=files.length?fs.readFileSync(home+'/Downloads/'+dirs[0]+'/prompt.md','utf8'):'';
 check('prompt about the wizard', /installer\/src\/wizard\.html/.test(md) && /Уменьшить/.test(md) && /не указано, выбери сам/.test(md), md.slice(0,300));
 const jpg=files.length?fs.readFileSync(home+'/Downloads/'+dirs[0]+'/shot-1.jpg'):Buffer.alloc(0);
 check('picture is a JPEG', jpg[0]===0xFF&&jpg[1]===0xD8, jpg.length);
 inst.kill();await b.close();console.log(fails?'FAILED '+fails:'all ok');process.exit(fails?1:0);
})();
