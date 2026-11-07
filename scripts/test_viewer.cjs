// NODE_PATH=/path/to/node_modules node scripts/test_viewer.cjs (jsdom)
const {JSDOM}=require('jsdom');
const fs=require('node:fs'),assert=require('node:assert/strict');
const media={matches:false,addEventListener(_,fn){this.changed=fn}};
const requests=[]; let stream,olderItems=[];
class Events {
  constructor(url){this.url=url;this.listeners={};stream=this;this.readyState=1}
  addEventListener(name,fn){this.listeners[name]=fn}
  close(){}
  emit(name,data,id=''){this.listeners[name]?.({data:JSON.stringify(data),lastEventId:String(id)})}
}
const dom=new JSDOM(fs.readFileSync('static/index.html','utf8'),{
  url:'https://viewer.example/',runScripts:'dangerously',pretendToBeVisual:true,
  beforeParse(w){
    w.matchMedia=()=>media;
    w.EventSource=Events;w.CSS={supports:()=>true};w.setInterval=()=>0;
    w.fetch=async url=>{requests.push(url);return {ok:true,status:200,text:async()=>url==='/healthz'?'ok':'',json:async()=>({items:url.startsWith('/history')?olderItems:[]})}};
    Object.defineProperties(w.HTMLElement.prototype,{
      scrollHeight:{get(){return this.classList.contains('channel-feed')?this.children.length*40:0}},
      clientHeight:{get(){return 160}},
      offsetTop:{get(){return this.parentElement?.classList.contains('channel-feed')?Array.from(this.parentElement.children).indexOf(this)*40:0}}
    });
  }
});
const w=dom.window,d=w.document,tick=()=>new Promise(r=>setTimeout(r,35));
function message(id,c=0){stream.emit('msg',{c,s:id,x:`交信 ${id}`},id)}
(async()=>{
  assert.equal(stream.url,'/stream?head=100');
  stream.emit('hello',{anchor:'2026-11-06T00:00:00Z',channels:[{id:0,name:'本部'},{id:1,name:'救護'}],live:false,seg:false});
  for(let i=100;i<106;i++)message(i,i%2);
  await tick();
  assert.equal(d.querySelectorAll('.channel').length,2);
  const feed=d.querySelector('.channel-feed[data-channel="0"]');
  assert.deepEqual(Array.from(feed.querySelectorAll('.r')).map(r=>+r.dataset.id),[100,102,104]);
  d.querySelector('.tab[data-v="1"]').click();
  assert.equal(feed.parentElement.hidden,true);
  d.querySelector('.tab[data-v="all"]').click();
  assert.equal(feed.parentElement.hidden,false);
  feed.scrollTop=40;feed.dispatchEvent(new w.WheelEvent('wheel',{deltaY:-10}));
  message(106);await tick();assert.equal(feed.scrollTop,40,'新着で過去の閲覧位置を動かさない');
  olderItems=[95,96,97,98,99].map(id=>({id,c:id%2,s:id,x:`以前の交信 ${id}`,g:1}));
  d.getElementById('older').click();await tick();
  assert(requests.includes('/history?before=100&limit=100'));
  assert.deepEqual(Array.from(feed.querySelectorAll('.r')).map(r=>+r.dataset.id),[96,98,100,102,104,106]);
  assert.equal(feed.scrollTop,120,'古い行の挿入分だけ位置を補正する');
  assert(d.querySelector('.r[data-id="96"] .pl'),'過去の交信も録音に関連付く');
  olderItems=[];d.getElementById('older').click();await tick();
  assert.equal(d.getElementById('older').disabled,true);
  d.getElementById('latest').click();await tick();
  for(let i=107;i<620;i++)message(i,0);
  await tick();assert.equal(d.querySelectorAll(".r").length,500);
  assert.equal(d.getElementById('older').disabled,false,'削除した履歴へ再度遡れる');
  const oldest=Math.min(...Array.from(d.querySelectorAll('.r[data-id]')).map(r=>+r.dataset.id));
  d.getElementById('older').click();await tick();
  assert(requests.includes(`/history?before=${oldest}&limit=100`));
  media.matches=true;media.changed();await tick();
  assert.equal(d.querySelectorAll('.channel-feed').length,1,'モバイルのすべては1列');
  const mixed=d.querySelector('.channel-feed');
  assert.equal(mixed.dataset.channel,'all');
  message(620,0);message(621,1);await tick();
  const mixedIds=Array.from(mixed.querySelectorAll('.r')).map(r=>+r.dataset.id);
  assert.deepEqual(mixedIds.slice(-2),[620,621],'異なるチャンネルを時刻順に混在');
  d.querySelector('.tab[data-v="1"]').click();await tick();
  assert.equal(d.querySelectorAll('.channel:not([hidden])').length,1);
  assert.equal(d.querySelector('.channel:not([hidden])').dataset.channel,'1');
  d.querySelector('.tab[data-v="all"]').click();await tick();
  assert.equal(d.querySelectorAll('.channel-feed').length,1);
  media.matches=false;media.changed();await tick();
  assert.equal(d.querySelectorAll('.channel-feed').length,2,'PCへ戻すとチャンネル別');
  assert.equal(d.querySelectorAll('.r[data-id="621"]').length,1,'切替で重複しない');
  console.log('PASS: channel columns, chronological order, history, scroll retention, pagination after pruning');
  dom.window.close();
})().catch(e=>{console.error(e);dom.window.close();process.exitCode=1});
