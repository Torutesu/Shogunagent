import {setupPrompt} from './setup-prompt.mjs';

const examples={
  launch:{project:'The website launch',decision:'Keep the homepage copy short and direct.',next:'Alex reviews the copy before we publish.',open:'The launch date hasn’t been confirmed.',source:'“Let’s keep it short and direct. Alex should review before we publish. Friday could work, but we haven’t agreed on a date.”',sourceName:'Sample project note · saved in the previous session',question:'Where are we with the website?',answer:'The copy should be short and direct. Alex still needs to review it before you publish.',uncertainty:'The launch date is still undecided.'},
  release:{project:'The next release',decision:'Keep the existing API response format.',next:'Mina runs the migration test before release.',open:'The cause of the slower query is unconfirmed.',source:'“Keep the response format as it is. Mina will run the migration test before we release. The index change might explain the slow query; we still need to profile it.”',sourceName:'Sample engineering note · saved in the previous session',question:'What’s left before we release?',answer:'Mina needs to run the migration test. Keep the API response format unchanged.',uncertainty:'The slow query still needs profiling. The index change is only a hypothesis.'},
  customer:{project:'The customer follow-up',decision:'Reproduce the export failure before promising a fix.',next:'Sam investigates and prepares an update.',open:'There’s no confirmed fix date yet.',source:'“Sam will reproduce the export failure and prepare an update. Don’t promise a fix until we understand it. We don’t have a date yet.”',sourceName:'Sample support note · saved in the previous session',question:'Can we update the customer?',answer:'Sam is investigating the export failure. Prepare an update after reproducing the issue.',uncertainty:'There’s no confirmed fix date to share yet.'}
};
const tabs=[...document.querySelectorAll('[data-scenario]')];
function selectExample(button){
  const example=examples[button.dataset.scenario];
  tabs.forEach(tab=>{const selected=tab===button;tab.setAttribute('aria-selected',String(selected));tab.tabIndex=selected?0:-1;});
  document.querySelector('#example-panel').setAttribute('aria-labelledby',button.id);
  document.querySelectorAll('[data-demo]').forEach(el=>{el.textContent=example[el.dataset.demo];});
  document.querySelector('.source-detail').open=false;
}
tabs.forEach((tab,index)=>{
  tab.addEventListener('click',()=>selectExample(tab));
  tab.addEventListener('keydown',event=>{
    const positions={ArrowRight:(index+1)%tabs.length,ArrowLeft:(index+tabs.length-1)%tabs.length,Home:0,End:tabs.length-1};
    if(!(event.key in positions))return;
    event.preventDefault();const next=tabs[positions[event.key]];selectExample(next);next.focus();
  });
});
document.querySelector('.source-jump').addEventListener('click',event=>{
  event.preventDefault();const detail=document.querySelector('.source-detail');detail.open=true;detail.querySelector('summary').focus();
});
const copy=document.querySelector('[data-setup-prompt]');
let copyTimer;
copy.addEventListener('click',async()=>{
  clearTimeout(copyTimer);
  try{await navigator.clipboard.writeText(setupPrompt(location.origin));copy.textContent='Copied — paste into your agent';}
  catch{copy.textContent='Copy unavailable — open full prompt';}
  copyTimer=setTimeout(()=>{copy.textContent='Copy setup prompt';},3500);
});
const menu=document.querySelector('.mobile-menu');
menu.addEventListener('click',event=>{if(event.target.closest('a'))menu.open=false;});
document.addEventListener('click',event=>{if(!menu.contains(event.target))menu.open=false;});
document.addEventListener('keydown',event=>{if(event.key==='Escape'&&menu.open){menu.open=false;menu.querySelector('summary').focus();}});
