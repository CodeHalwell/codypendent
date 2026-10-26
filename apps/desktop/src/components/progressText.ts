export function progressText(text: string): string {
  const notes = text.trim().split(/\n\s*\n/);
  return notes.every((note) => /^\*\*[^*\n]{1,140}\*\*$/.test(note))
    ? notes.map((note) => note.slice(2, -2)).join(" · ")
    : text;
}
