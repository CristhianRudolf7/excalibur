function(selector,text,loggedIn){
 const visible=e=>{if(!e||e.nodeType!==1)return false;const b=e.getBoundingClientRect(),s=getComputedStyle(e);return b.width>0&&b.height>0&&s.display!=='none'&&s.visibility!=='hidden'&&Number(s.opacity)>0};
 if(!globalThis.__excaliburReady){
  const s=globalThis.__excaliburReady={mutation:performance.now(),beat:performance.now()};
  const observer=new MutationObserver(records=>{let relevant=false;for(const r of records){if(r.type==='attributes'&&r.attributeName==='style'){const b=r.target.getBoundingClientRect();if(b.bottom<0||b.top>innerHeight||b.right<0||b.left>innerWidth)continue}relevant=true;break}if(relevant){s.mutation=performance.now();try{globalThis.__excalibur_observe('mutation')}catch{}}});
  observer.observe(document,{childList:true,subtree:true,attributes:true,characterData:true});
  const beat=()=>{s.beat=performance.now();requestAnimationFrame(beat)};requestAnimationFrame(beat);
 }
 const s=globalThis.__excaliburReady;let condition=!selector&&!text;
 const roots=[document];for(let i=0;i<roots.length;i++)for(const e of roots[i].querySelectorAll('*'))if(e.shadowRoot){roots.push(e.shadowRoot);if(!e.shadowRoot.__excaliburObserved){e.shadowRoot.__excaliburObserved=true;new MutationObserver(()=>s.mutation=performance.now()).observe(e.shadowRoot,{childList:true,subtree:true,attributes:true,characterData:true});}}
 if(selector){for(const root of roots)try{if([...root.querySelectorAll(selector)].some(visible))condition=true}catch{}}
 if(text){for(const root of roots)if([...root.querySelectorAll('*')].some(e=>visible(e)&&(e.innerText||'').includes(text)))condition=true}
 let blocked=null;for(const f of document.querySelectorAll('iframe')){const b=f.getBoundingClientRect();if(visible(f)&&b.width*b.height>256&&/challenges\.cloudflare\.com|hcaptcha\.com|arkoselabs\.com|funcaptcha\.com|captcha-delivery\.com|(?:google\.com|recaptcha\.net)\/recaptcha\/(?:api2|enterprise)\/anchor/.test(f.src))blocked={kind:'captcha',vendor:new URL(f.src).hostname,signals:['visible_challenge_frame'],confidence:'high',url:location.href};}
 if(!blocked)for(const e of document.querySelectorAll('.g-recaptcha,.h-captcha,.cf-turnstile,[data-sitekey]'))if(visible(e)){blocked={kind:'captcha',vendor:'site',signals:['visible_challenge_marker'],confidence:'medium',url:location.href};break}
 const content=(document.body?.innerText||'').slice(0,16384);if(!blocked&&/Access to this page has been denied|Too many requests|Couldn't sign you in/.test(content))blocked={kind:'blocked_by_site',vendor:location.hostname,signals:['block_page_text'],confidence:'medium',url:location.href};
 if(!blocked&&/^(accounts\.google\.com|login\.microsoftonline\.com|login\.live\.com)$/.test(location.hostname))blocked={kind:'needs_login',vendor:location.hostname,signals:['identity_origin'],confidence:'high',url:location.href};
 if(!blocked&&[...document.querySelectorAll('input[autocomplete=one-time-code],input[maxlength="1"][inputmode=numeric]')].some(e=>visible(e)&&!e.value))blocked={kind:'needs_2fa',vendor:location.hostname,signals:['empty_verification_field'],confidence:'medium',url:location.href};
 if(!blocked&&loggedIn&&[...document.querySelectorAll('input[type=password]')].some(visible))blocked={kind:'needs_login',vendor:location.hostname,signals:['password_on_logged_in_site'],confidence:'medium',url:location.href};
 return {url:location.href,ready:document.readyState,dom_age:performance.now()-s.mutation,beat_age:performance.now()-s.beat,condition,blocked};
}
