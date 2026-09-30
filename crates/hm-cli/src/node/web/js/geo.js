// Places on the Earth: Maidenhead locators, Web Mercator, great circles.

import { WORLD } from "./land.js";

export { WORLD };

const MAX_LAT = 85.0511287798;
const EARTH_KM = 6371;
const rad = (deg) => (deg * Math.PI) / 180;
const deg = (r) => (r * 180) / Math.PI;

const LOCATOR = /^[A-R]{2}[0-9]{2}([A-X]{2})?$/i;

/** A locator as people write it (field and square upper case, subsquare lower), or null. */
export function formatLocator(text) {
  const t = (text || "").trim();
  if (!LOCATOR.test(t)) return null;
  const u = t.toUpperCase();
  return u.length === 6 ? u.slice(0, 4) + u.slice(4).toLowerCase() : u;
}

/** The centre of a grid square: {lat, lon}, or null. */
export function locatorCentre(text) {
  const t = formatLocator(text);
  if (!t) return null;
  const b = t.toUpperCase();
  let lon = -180 + (b.charCodeAt(0) - 65) * 20 + (b.charCodeAt(2) - 48) * 2;
  let lat = -90 + (b.charCodeAt(1) - 65) * 10 + (b.charCodeAt(3) - 48);
  if (b.length === 6) {
    lon += (b.charCodeAt(4) - 65) * (2 / 24) + 1 / 24;
    lat += (b.charCodeAt(5) - 65) * (1 / 24) + 0.5 / 24;
  } else {
    lon += 1;
    lat += 0.5;
  }
  return { lat, lon };
}

/** The six-character square a place is in. */
export function toLocator(lat, lon) {
  const x = Math.min(359.999999, Math.max(0, lon + 180));
  const y = Math.min(179.999999, Math.max(0, lat + 90));
  return (
    String.fromCharCode(65 + Math.floor(x / 20), 65 + Math.floor(y / 10)) +
    Math.floor((x % 20) / 2) +
    Math.floor(y % 10) +
    String.fromCharCode(97 + Math.floor((x % 2) * 12), 97 + Math.floor((y % 1) * 24))
  );
}

/** Web Mercator: a place in world units, x east and y south, 0 to WORLD. */
export function project(lat, lon) {
  const phi = rad(Math.max(-MAX_LAT, Math.min(MAX_LAT, lat)));
  return [
    ((lon + 180) / 360) * WORLD,
    ((1 - Math.log(Math.tan(Math.PI / 4 + phi / 2)) / Math.PI) / 2) * WORLD,
  ];
}

/** The place at world units (x, y). */
export function unproject(x, y) {
  const n = Math.PI * (1 - (2 * y) / WORLD);
  return { lat: deg(Math.atan(Math.sinh(n))), lon: (x / WORLD) * 360 - 180 };
}

/** Great-circle distance in km and initial bearing in degrees from north. */
export function distance(a, b) {
  const [p1, p2] = [rad(a.lat), rad(b.lat)];
  const dl = rad(b.lon - a.lon);
  const h = Math.sin((p2 - p1) / 2) ** 2 + Math.cos(p1) * Math.cos(p2) * Math.sin(dl / 2) ** 2;
  const km = 2 * EARTH_KM * Math.asin(Math.min(1, Math.sqrt(h)));
  const y = Math.sin(dl) * Math.cos(p2);
  const x = Math.cos(p1) * Math.sin(p2) - Math.sin(p1) * Math.cos(p2) * Math.cos(dl);
  return { km, bearing: (deg(Math.atan2(y, x)) + 360) % 360 };
}

/** Points along the great circle from a to b, for drawing long paths. */
export function greatCircle(a, b, steps = 32) {
  const [p1, l1, p2, l2] = [rad(a.lat), rad(a.lon), rad(b.lat), rad(b.lon)];
  const d = 2 * Math.asin(Math.sqrt(
    Math.sin((p2 - p1) / 2) ** 2 + Math.cos(p1) * Math.cos(p2) * Math.sin((l2 - l1) / 2) ** 2));
  if (d < 1e-9) return [a, b];
  const out = [];
  for (let i = 0; i <= steps; i++) {
    const f = i / steps;
    const s1 = Math.sin((1 - f) * d) / Math.sin(d);
    const s2 = Math.sin(f * d) / Math.sin(d);
    const x = s1 * Math.cos(p1) * Math.cos(l1) + s2 * Math.cos(p2) * Math.cos(l2);
    const y = s1 * Math.cos(p1) * Math.sin(l1) + s2 * Math.cos(p2) * Math.sin(l2);
    const z = s1 * Math.sin(p1) + s2 * Math.sin(p2);
    out.push({ lat: deg(Math.atan2(z, Math.hypot(x, y))), lon: deg(Math.atan2(y, x)) });
  }
  return out;
}

/** A place picked at random within `km` of {lat, lon}, uniformly over the disc. */
export function fuzz(place, km) {
  if (km <= 0) return place;
  const bearing = Math.random() * 2 * Math.PI;
  const r = Math.sqrt(Math.random()) * km;
  const cos = Math.cos(rad(place.lat));
  return {
    lat: Math.max(-90, Math.min(90, place.lat + (r / 111.32) * Math.cos(bearing))),
    lon: ((place.lon + (cos < 1e-6 ? 0 : (r / (111.32 * cos)) * Math.sin(bearing)) + 540) % 360) - 180,
  };
}
