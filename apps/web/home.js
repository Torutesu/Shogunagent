import {setupPrompt} from './setup-prompt.mjs';

const examples={
  launch:{project:'The website launch',decision:'Keep the homepage copy short and direct.',next:'Alex reviews the copy before we publish.',open:'The launch date hasn’t been confirmed.',source:'“Let’s keep it short and direct. Alex should review before we publish. Friday could work, but we haven’t agreed on a date.”',sourceName:'Sample project note · saved in the previous session',question:'Where are we with the website?',answer:'The copy should be short and direct. Alex still needs to review it before you publish.',uncertainty:'The launch date is still undecided.'},
  release:{project:'The next release',decision:'Keep the existing API response format.',next:'Mina runs the migration test before release.',open:'The cause of the slower query is unconfirmed.',source:'“Keep the response format as it is. Mina will run the migration test before we release. The index change might explain the slow query; we still need to profile it.”',sourceName:'Sample engineering note · saved in the previous session',question:'What’s left before we release?',answer:'Mina needs to run the migration test. Keep the API response format unchanged.',uncertainty:'The slow query still needs profiling. The index change is only a hypothesis.'},
  customer:{project:'The customer follow-up',decision:'Reproduce the export failure before promising a fix.',next:'Sam investigates and prepares an update.',open:'There’s no confirmed fix date yet.',source:'“Sam will reproduce the export failure and prepare an update. Don’t promise a fix until we understand it. We don’t have a date yet.”',sourceName:'Sample support note · saved in the previous session',question:'Can we update the customer?',answer:'Sam is investigating the export failure. Prepare an update after reproducing the issue.',uncertainty:'There’s no confirmed fix date to share yet.'}
};

// Each block wires one independent component, so a missing element disables only that component.
const panel=document.querySelector('#example-panel');
const sourceDetail=document.querySelector('.source-detail');
const tabs=[...document.querySelectorAll('[data-scenario]')];
function selectExample(button){
  const example=examples[button.dataset.scenario];
  if(!example)return;
  tabs.forEach(tab=>{const selected=tab===button;tab.setAttribute('aria-selected',String(selected));tab.tabIndex=selected?0:-1;});
  panel?.setAttribute('aria-labelledby',button.id);
  document.querySelectorAll('[data-demo]').forEach(el=>{el.textContent=example[el.dataset.demo];});
  if(sourceDetail)sourceDetail.open=false;
}
tabs.forEach((tab,index)=>{
  tab.addEventListener('click',()=>selectExample(tab));
  tab.addEventListener('keydown',event=>{
    const positions={ArrowRight:(index+1)%tabs.length,ArrowLeft:(index+tabs.length-1)%tabs.length,Home:0,End:tabs.length-1};
    if(!(event.key in positions))return;
    event.preventDefault();const next=tabs[positions[event.key]];selectExample(next);next.focus();
  });
});

const sourceJump=document.querySelector('.source-jump');
if(sourceJump&&sourceDetail){
  sourceJump.addEventListener('click',event=>{
    event.preventDefault();sourceDetail.open=true;sourceDetail.querySelector('summary')?.focus();
  });
}

// Copy the prompt for this page's origin so local previews point at their own guides.
const copy=document.querySelector('[data-setup-prompt]');
const fallback=document.querySelector('[data-setup-fallback]');
if(copy){
  let copyTimer;
  copy.addEventListener('click',async()=>{
    clearTimeout(copyTimer);
    let prompt;
    try{
      prompt=setupPrompt(location.origin);
      await navigator.clipboard.writeText(prompt);
      if(fallback)fallback.hidden=true;
      copy.textContent='Copied — paste into your agent';
      copyTimer=setTimeout(()=>{copy.textContent='Copy setup prompt';},3500);
    }catch{
      // Clipboard access can be denied (insecure origin, permissions). Show the prompt selected
      // so it can be copied by hand; keep the message until the next attempt.
      if(fallback){
        fallback.value=prompt??setupPrompt();
        fallback.hidden=false;
        fallback.focus();
        fallback.select();
        copy.textContent='Copy blocked — prompt selected below';
      }else copy.textContent='Copy unavailable — open full prompt';
    }
  });
}

const menu=document.querySelector('.mobile-menu');
if(menu){
  menu.addEventListener('click',event=>{if(event.target.closest('a'))menu.open=false;});
  document.addEventListener('click',event=>{if(!menu.contains(event.target))menu.open=false;});
  document.addEventListener('keydown',event=>{if(event.key==='Escape'&&menu.open){menu.open=false;menu.querySelector('summary')?.focus();}});
}
