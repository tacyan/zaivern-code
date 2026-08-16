'use strict';
const qs = new URLSearchParams(location.search);
let TOK = qs.get('t') || localStorage.getItem('zv_tok') || '';
if (qs.get('t')) localStorage.setItem('zv_tok', qs.get('t'));
const $ = id => document.getElementById(id);
// ─── 多言語 (Language Pack) ───
