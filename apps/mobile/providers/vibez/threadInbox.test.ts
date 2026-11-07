import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {test} from 'node:test';

import type {Session} from '../../lib/auth.ts';
import {RestClient} from '../../lib/rest.ts';
import {createWriteQueue} from '../../db/writeQueue.ts';
import {createVibezProvider} from './index.ts';
import {NativeStore} from './store.ts';
import {nativeTestDatabase} from './testDatabase.ts';
import {threadInboxPage} from './threadInbox.ts';
import {NativeTransport} from './transport.ts';
import {decodeNative} from './validation.ts';
import type {Message,ThreadInboxEntry} from './protocol.generated.ts';

const fixture=JSON.parse(readFileSync(new URL('../../../../docs/protocol/v1.fixture.json',import.meta.url),'utf8'));
const room=decodeNative('Room',fixture.room);
const message=decodeNative('Message',fixture.message);
const account:Session={kind:'vibez',baseUrl:'http://localhost:3400',authToken:'test-token',userId:'alice-id',username:'alice',siteUrl:null,nativeInstanceId:fixture.discovery.instance_id,nativeDataEpoch:fixture.discovery.data_epoch};

const person=(id:string)=>({id,username:id,display_name:id});
const root=(id:string,over:Partial<Message>={}):Message=>({...message,id,author:{...message.author,id:'bob-id',username:'bob'},text:`root ${id}`,...over});
const entry=(id:string,over:Partial<ThreadInboxEntry>={}):ThreadInboxEntry=>({
  following:true,last_reply_at:'2026-11-01T10:00:00Z',last_reply_position:'9',mentions:'0',participants:[],replies:'3',room_id:room.id,root:root(id),unread:'0',...over,
});

type Sent={method:string;path:string;body:unknown};
function server(routes:(sent:Sent)=>unknown,capabilities:Record<string,boolean>={thread_inbox:true,thread_follow:true}){
  const sent:Sent[]=[];
  class Fake extends NativeTransport {
    override discover=async()=>({...fixture.discovery,capabilities:{...fixture.discovery.capabilities,...capabilities}});
    override me=async()=>({...fixture.session.user,id:account.userId});
    override changes=async()=>({protocol_version:1,changes:[],cursor:'next',has_more:false});
    override socketUrl=async()=>'ws://localhost/native';
  }
  const fetcher=(async(input:RequestInfo|URL,init?:RequestInit)=>{
    const url=new URL(String(input));
    const call={method:init?.method??'GET',path:url.pathname+url.search,body:init?.body?JSON.parse(String(init.body)):undefined};
    sent.push(call);
    return new Response(JSON.stringify(routes(call)),{status:200,headers:{'Content-Type':'application/json'}});
  }) as typeof fetch;
  const transport=new Fake(account.baseUrl,fetcher);
  transport.restore(account.authToken);
  return {sent,transport};
}

async function connected(routes:(sent:Sent)=>unknown,capabilities?:Record<string,boolean>){
  const {sent,transport}=server(routes,capabilities);
  const {db,adapter}=nativeTestDatabase();
  const store=new NativeStore(adapter,createWriteQueue(),account);
  await store.applySnapshot({protocol_version:1,rooms:[room],messages:[],cursor:'initial'});
  let socket:WebSocket|undefined;
  let id=0;
  const provider=createVibezProvider(account,new RestClient(account.baseUrl),()=>`op-${++id}`,store,{transport,socket:()=>{
    socket={readyState:1,onopen:null,onclose:null,onerror:null,onmessage:null,close:()=>{}} as unknown as WebSocket;
    queueMicrotask(()=>socket!.onopen?.(new Event('open')));
    return socket;
  }});
  return {provider,sent,socket:()=>socket!,stop:()=>{provider.native!.chat.stop();db.close();}};
}

test('the actions stay absent until the server announces them',async()=>{
  const off=await connected(()=>({}),{thread_inbox:false,thread_follow:false});
  try{
    assert.equal(off.provider.actions.threadInbox,undefined,'before connecting');
    await off.provider.native!.chat.connect();
    for(const action of ['threadInbox','threadTotals','markAllThreadsRead','followThread','threadFollowing','onThreadsChanged'] as const){
      assert.equal(off.provider.actions[action],undefined,action);
    }
  }finally{off.stop();}
  const on=await connected(()=>({}));
  try{
    await on.provider.native!.chat.connect();
    for(const action of ['threadInbox','threadTotals','markAllThreadsRead','followThread','threadFollowing','onThreadsChanged'] as const){
      assert.equal(typeof on.provider.actions[action],'function',action);
    }
    assert.equal(on.provider.actions.markThreadRead,undefined,'the thread screen reads by observed reply');
  }finally{on.stop();}
});

test('following alone: the bell without the list',async()=>{
  const only=await connected(()=>({}),{thread_inbox:false,thread_follow:true});
  try{
    await only.provider.native!.chat.connect();
    assert.equal(only.provider.actions.threadInbox,undefined);
    assert.equal(typeof only.provider.actions.followThread,'function');
    assert.equal(typeof only.provider.actions.onThreadsChanged,'function');
  }finally{only.stop();}
});

test('a page reads its route, names the room and maps the counters',async()=>{
  const t=await connected(({path})=>{
    if(path.startsWith('/api/v1/threads?'))return {threads:[entry('r1',{unread:'2',mentions:'1',replies:'7',participants:[person('bob-id')]})],next:'41'};
    return {};
  });
  try{
    await t.provider.native!.chat.connect();
    const page=await t.provider.actions.threadInbox!('unread',null);
    assert.ok(t.sent.some(call=>call.method==='GET'&&call.path==='/api/v1/threads?filter=unread'));
    assert.equal(page.next,'41');
    const [first]=page.entries;
    assert.equal(first?.root.id,'r1');
    assert.equal(first?.room,room.name);
    assert.deepEqual([first?.lastReply,first?.replies,first?.unreadReplies,first?.unreadMentions],[Date.parse('2026-11-01T10:00:00Z'),7,2,1]);
    assert.deepEqual(first?.participants,[{id:'bob-id',username:'bob-id'}]);
    await t.provider.actions.threadInbox!('followed','41');
    assert.ok(t.sent.some(call=>call.path==='/api/v1/threads?filter=followed&before=41'));
  }finally{t.stop();}
});

test('participated and mentioned, each root once',async()=>{
  const direct={name:'bob',direct:true},plain={name:'dev',direct:false};
  const {entries,next}=await threadInboxPage({threads:[
    entry('plain'),
    entry('plain'),
    entry('mine',{root:root('mine',{author:{...message.author,id:account.userId,username:'alice'}})}),
    entry('replied',{participants:[person(account.userId)]}),
    entry('dm',{room_id:'dm'}),
    entry('counted',{mentions:'1'}),
    entry('named',{root:root('named',{text:'hello @alice!'})}),
    entry('near',{root:root('near',{text:'@alicia @all'})}),
  ]},{id:account.userId,username:'alice'},async rid=>rid==='dm'?direct:plain);
  assert.equal(next,null);
  assert.deepEqual(Object.fromEntries(entries.map(e=>[e.root.id,[e.participated,e.mentioned]])),{
    plain:[false,false],mine:[true,false],replied:[true,false],dm:[true,false],counted:[false,true],named:[false,true],near:[false,false],
  });
  assert.equal(entries.length,7);
});

test('a room the store does not know keeps an empty name',async()=>{
  const {entries}=await threadInboxPage({threads:[entry('r')]},{id:account.userId,username:'alice'},async()=>null);
  assert.deepEqual([entries[0]?.room,entries[0]?.participated],['',false]);
});

test('totals, mark all, follow and the following answer use the server routes',async()=>{
  let followed=false;
  const t=await connected(({method,path,body})=>{
    if(path==='/api/v1/threads/totals')return {unread_threads:'4',unread_mentions:'2'};
    if(path==='/api/v1/threads/read')return {operation_id:(body as {operation_id:string}).operation_id,threads:'4',applied_revision:'12'};
    if(path==='/api/v1/messages/r1/follow'){followed=(body as {present:boolean}).present;return root('r1',{personal_follow:{present:followed,revision:'5'}});}
    if(path==='/api/v1/messages/r1'&&method==='GET')return root('r1',{personal_follow:{present:followed,revision:'5'}});
    return {};
  });
  try{
    await t.provider.native!.chat.connect();
    const actions=t.provider.actions;
    let changed=0;
    const release=actions.onThreadsChanged!(()=>{changed++;});
    assert.deepEqual(await actions.threadTotals!(),{unreadThreads:4,unreadMentions:2});
    await actions.markAllThreadsRead!();
    const marked=t.sent.find(call=>call.path==='/api/v1/threads/read');
    assert.equal(marked?.method,'POST');
    assert.match((marked?.body as {operation_id:string}).operation_id,/^op-/);
    assert.equal(changed,1);
    assert.equal(await actions.threadFollowing!(room.id,'r1'),false);
    await actions.followThread!(room.id,'r1',true);
    const follow=t.sent.find(call=>call.path==='/api/v1/messages/r1/follow');
    assert.equal(follow?.method,'PUT');
    assert.equal((follow?.body as {present:boolean}).present,true);
    assert.equal(await actions.threadFollowing!(room.id,'r1'),true);
    assert.equal(changed,2);
    release();
    await actions.followThread!(room.id,'r1',false);
    assert.equal(changed,2);
  }finally{t.stop();}
});

test('a journal batch tells the list to read again',async()=>{
  const t=await connected(()=>({}));
  try{
    await t.provider.native!.chat.connect();
    let changed=0;
    t.provider.actions.onThreadsChanged!(()=>{changed++;});
    const batch={protocol_version:1,changes:[],cursor:'moved',has_more:false,type:'batch'};
    await t.socket().onmessage?.(new MessageEvent('message',{data:JSON.stringify(batch)}));
    for(let i=0;i<50&&changed===0;i++)await new Promise(resolve=>setTimeout(resolve,10));
    assert.equal(changed,1);
  }finally{t.stop();}
});
