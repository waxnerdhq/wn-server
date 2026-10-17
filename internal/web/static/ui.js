// Copy buttons and confirmation prompts for wn-server's web UI.
document.querySelectorAll("[data-copy]").forEach(function (btn) {
  btn.addEventListener("click", function () {
    var input = document.getElementById(btn.getAttribute("data-copy"));
    var done = function () {
      btn.textContent = "Copied";
      setTimeout(function () { btn.textContent = "Copy"; }, 2000);
    };
    // The clipboard API needs HTTPS or localhost. On plain HTTP the text is
    // selected and copied with execCommand.
    var fallback = function () {
      input.select();
      try { if (document.execCommand("copy")) { done(); } } catch (e) {}
    };
    if (navigator.clipboard && window.isSecureContext) {
      navigator.clipboard.writeText(input.value).then(done, fallback);
    } else {
      fallback();
    }
  });
});
document.querySelectorAll("form[data-confirm]").forEach(function (form) {
  form.addEventListener("submit", function (e) {
    if (!window.confirm(form.getAttribute("data-confirm"))) { e.preventDefault(); }
  });
});
