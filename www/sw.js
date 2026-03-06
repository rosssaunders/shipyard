// Service Worker for PWA install + push notifications
const CACHE = 'shipyard-v2';
const ASSETS = ['/', '/index.html', '/manifest.json'];

self.addEventListener('install', (e) => {
    e.waitUntil(caches.open(CACHE).then(c => c.addAll(ASSETS)));
    self.skipWaiting();
});

self.addEventListener('activate', (e) => {
    // Delete old caches
    e.waitUntil(
        caches.keys().then(keys =>
            Promise.all(keys.filter(k => k !== CACHE).map(k => caches.delete(k)))
        ).then(() => self.clients.claim())
    );
});

self.addEventListener('fetch', (e) => {
    // Network first, cache fallback
    e.respondWith(
        fetch(e.request).catch(() => caches.match(e.request))
    );
});

self.addEventListener('push', (e) => {
    const data = e.data?.json() || { title: '⚓ Shipyard', body: 'Agent update' };
    e.waitUntil(
        self.registration.showNotification(data.title, {
            body: data.body,
            icon: "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><text y='.9em' font-size='90'>⚓</text></svg>",
            badge: "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><text y='.9em' font-size='90'>⚓</text></svg>",
        })
    );
});
