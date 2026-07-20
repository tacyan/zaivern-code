/* 選択結果が確定したかを問い合わせる。未確定なら空文字を返す。 */
(function () {
  return window.__zvPick ? JSON.stringify(window.__zvPick) : "";
})();
