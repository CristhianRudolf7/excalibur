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
 const corroborated=()=>{const hay=((document.title||'')+'\n'+(document.body?.innerText||'')).slice(0,16384);return /verify you are human|just a moment|verifica que eres humano/i.test(hay)};
 const addCorroboration=(block)=>{if(block&&corroborated()&&!block.signals.includes('corroboration_text'))block.signals.push('corroboration_text');return block};
 let blocked=null;for(const f of document.querySelectorAll('iframe')){const b=f.getBoundingClientRect();if(visible(f)&&b.width*b.height>256&&/challenges\.cloudflare\.com|hcaptcha\.com|arkoselabs\.com|funcaptcha\.com|captcha-delivery\.com|(?:google\.com|recaptcha\.net)\/recaptcha\/(?:api2|enterprise)\/anchor/.test(f.src)){try{blocked={kind:'blocked_captcha',vendor:new URL(f.src).hostname,signals:['visible_challenge_frame'],confidence:'high',url:location.href}}catch{blocked={kind:'blocked_captcha',vendor:'challenge',signals:['visible_challenge_frame'],confidence:'high',url:location.href}}break}}
 if(!blocked)for(const e of document.querySelectorAll('.g-recaptcha,.h-captcha,.cf-turnstile,[data-sitekey],textarea[name=g-recaptcha-response]'))if(visible(e)){blocked={kind:'blocked_captcha',vendor:'site',signals:['visible_challenge_marker'],confidence:'medium',url:location.href};break}
 const content=(document.body?.innerText||'').slice(0,16384);
 const perimeter=document.querySelector('meta[name=description][content=px-captcha],meta[name="description"][content="px-captcha"]');
 if(!blocked&&(perimeter||/Access to this page has been denied/.test(content)))blocked={kind:'blocked_by_site',vendor:location.hostname,signals:perimeter?['perimeter_block_page']:['block_page_text'],confidence:'medium',url:location.href};
 if(!blocked&&/Too many requests/.test(content))blocked={kind:'blocked_by_site',vendor:location.hostname,signals:['block_page_text'],confidence:'medium',url:location.href};
 if(!blocked&&/Couldn't sign you in/.test(content))blocked={kind:'blocked_by_site',vendor:location.hostname,signals:['signin_rejected'],confidence:'medium',url:location.href};
 if(!blocked&&(/^\/sorry\//.test(location.pathname)||/unusual traffic|tráfico inusual/i.test(content)))blocked={kind:'blocked_by_site',vendor:location.hostname,signals:['google_interstitial'],confidence:'medium',url:location.href};
 if(!blocked&&/^(accounts\.google\.com|login\.microsoftonline\.com|login\.live\.com)$/.test(location.hostname))blocked={kind:'needs_login',vendor:location.hostname,signals:['identity_origin'],confidence:'high',url:location.href};
 if(!blocked&&/web\.whatsapp\.com$/.test(location.hostname)&&/Scan to log in|Escanea.*QR|Vincula.*teléfono/i.test(content)&&document.querySelector('canvas'))blocked={kind:'needs_login',vendor:'web.whatsapp.com',signals:['qr_login_screen'],confidence:'high',url:location.href};
 if(!blocked&&[...document.querySelectorAll('input[autocomplete=one-time-code]')].some(e=>visible(e)&&!e.value))blocked={kind:'needs_2fa',vendor:location.hostname,signals:['empty_verification_field'],confidence:'medium',url:location.href};
 if(!blocked){const boxes=[...document.querySelectorAll('input[maxlength="1"][inputmode=numeric]')].filter(e=>visible(e)&&!e.value);if(boxes.length>=4)blocked={kind:'needs_2fa',vendor:location.hostname,signals:['otp_group'],confidence:'medium',url:location.href}}
 if(!blocked&&loggedIn&&[...document.querySelectorAll('input[type=password]')].some(visible))blocked={kind:'needs_login',vendor:location.hostname,signals:['password_on_logged_in_site'],confidence:'medium',url:location.href};
 if(!blocked){const wall=[...document.querySelectorAll('#cookie-wall,.cookie-wall,#consent-wall,.consent-wall,#consent-gate,.consent-gate,[data-cookie-wall]')].find(visible);if(wall&&/we value your privacy|we use cookies|accept.*cookies|cookie.*consent|consent.*cookies/i.test((wall.innerText||'')+'\n'+content))blocked={kind:'blocked_cookie_wall',vendor:location.hostname,signals:['cookie_wall_block'],confidence:'medium',url:location.href}}
 if(!blocked&&/not available in your (country|region)|blocked in your (country|region)|access[^.]{0,40}not available[^.]{0,40}region|geo-block|error 451|451 unavailable/i.test(content))blocked={kind:'blocked_geo',vendor:location.hostname,signals:['geo_block_text'],confidence:'medium',url:location.href};
 blocked=addCorroboration(blocked);
 return {url:location.href,ready:document.readyState,dom_age:performance.now()-s.mutation,beat_age:performance.now()-s.beat,condition,blocked};
}
