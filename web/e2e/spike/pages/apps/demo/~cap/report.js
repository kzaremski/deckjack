// Module scripts run in document order after the classic one, and a module
// that was blocked or failed CORS never runs. A beat later, report what ran
// and what this document is, to the parent.
setTimeout(() => {
  let cookie;
  try {
    cookie = document.cookie;
  } catch (e) {
    cookie = `throws ${e.name}`;
  }
  parent.postMessage({ ran: self.ran ?? [], origin: String(self.origin), cookie }, "*");
}, 300);
