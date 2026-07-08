package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func setup(t *testing.T) string {
	t.Helper()
	root := t.TempDir()
	tasks := filepath.Join(root, "docs", "tasks")
	if err := os.MkdirAll(tasks, 0o755); err != nil {
		t.Fatal(err)
	}
	files := map[string]string{
		boardPath(root):                   fixtureBoard,
		archivePath(root):                 fixtureArchive,
		filepath.Join(tasks, "XR-005.md"): "# XR-005\n",
		filepath.Join(tasks, "XR-002.md"): "# XR-002\n",
	}
	for p, content := range files {
		if err := os.WriteFile(p, []byte(content), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	return root
}

func backlogIDs(t *testing.T, root string) []string {
	t.Helper()
	b, err := LoadBoard(boardPath(root))
	if err != nil {
		t.Fatal(err)
	}
	var ids []string
	for _, r := range b.Sects[SectBacklog].Rows {
		ids = append(ids, r.ID)
	}
	return ids
}

func TestNextID(t *testing.T) {
	root := setup(t)
	id, err := cmdID(root)
	if err != nil {
		t.Fatal(err)
	}
	if id != "XR-008" {
		t.Fatalf("ожидал XR-008, получил %s", id)
	}
}

func TestAddSorted(t *testing.T) {
	root := setup(t)
	// Равный R с XR-002: новая строка с большим номером встаёт ниже.
	if _, err := cmdAdd(root, AddParams{Title: "Равный ранг", Type: "bug", Rank: "50+0+0+5+0", Link: "x"}); err != nil {
		t.Fatal(err)
	}
	// Максимальный ранг встаёт первым, минимальный последним.
	if _, err := cmdAdd(root, AddParams{ID: "XR-020", Title: "Наверх", Type: "task", Rank: "75+0+1+0+0", Link: "x"}); err != nil {
		t.Fatal(err)
	}
	if _, err := cmdAdd(root, AddParams{ID: "XR-021", Title: "В хвост", Type: "task", Rank: "0+0+1+0+0", Link: "x"}); err != nil {
		t.Fatal(err)
	}
	got := strings.Join(backlogIDs(t, root), " ")
	want := "XR-020 XR-002 XR-008 XR-001 XR-003 XR-004 XR-021"
	if got != want {
		t.Fatalf("порядок Backlog: %s, ожидал %s", got, want)
	}
}

func TestAddValidation(t *testing.T) {
	root := setup(t)
	cases := []AddParams{
		{Title: "Без ссылки и файла", Type: "task", Rank: "0+1+1+0+1"},
		{Title: "Дубль", Type: "task", Rank: "0+1+1+0+1", Link: "x", ID: "XR-007"},
		{Title: "С|пайпом", Type: "task", Rank: "0+1+1+0+1", Link: "x"},
		{Title: "Плохой тип", Type: "feature", Rank: "0+1+1+0+1", Link: "x"},
		{Title: "Плохой статус", Type: "task", Rank: "0+1+1+0+1", Link: "x", Status: "done"},
	}
	for _, p := range cases {
		if _, err := cmdAdd(root, p); err == nil {
			t.Errorf("ожидал ошибку на %+v", p)
		}
	}
}

func TestMoveToBlockedAndBack(t *testing.T) {
	root := setup(t)
	if _, err := cmdMove(root, "XR-004", SectBlocked, ""); err == nil {
		t.Fatal("blocked без --reason должен падать")
	}
	if _, err := cmdMove(root, "XR-004", SectBlocked, "ждём железо"); err != nil {
		t.Fatal(err)
	}
	b, err := LoadBoard(boardPath(root))
	if err != nil {
		t.Fatal(err)
	}
	rows := b.Sects[SectBlocked].Rows
	if len(rows) != 1 || !strings.Contains(rows[0].Title, "[блок: ждём железо]") {
		t.Fatalf("Blocked после move: %+v", rows)
	}
	if _, err := cmdMove(root, "XR-004", SectBlocked, "повтор"); err == nil {
		t.Fatal("повторный move в ту же секцию должен падать")
	}
	if _, err := cmdMove(root, "XR-004", SectBacklog, ""); err != nil {
		t.Fatal(err)
	}
	ids := backlogIDs(t, root)
	if ids[len(ids)-1] != "XR-004" {
		t.Fatalf("XR-004 должен вернуться в хвост Backlog: %v", ids)
	}
}

func TestClose(t *testing.T) {
	root := setup(t)
	msg, err := cmdClose(root, CloseParams{ID: "XR-005", Commits: "deadbee", Date: "2026-07-08"})
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(msg, "tasks/archive/2026/XR-005.md") {
		t.Fatalf("сообщение без пути архива: %s", msg)
	}
	if _, err := os.Stat(filepath.Join(root, "docs", "tasks", "archive", "2026", "XR-005.md")); err != nil {
		t.Fatal("файл задачи не переехал в архив:", err)
	}
	if _, err := os.Stat(filepath.Join(root, "docs", "tasks", "XR-005.md")); !os.IsNotExist(err) {
		t.Fatal("файл задачи остался на старом месте")
	}
	board, _ := os.ReadFile(boardPath(root))
	rowLine := "| XR-005 | Задача в работе | task | P2 | 30 (25+2+1+0+2) | [tasks/XR-005.md](tasks/XR-005.md) |\n"
	if want := strings.Replace(fixtureBoard, rowLine, "", 1); string(board) != want {
		t.Fatalf("доска после close отличается не только строкой XR-005:\n%s", board)
	}
	arch, _ := os.ReadFile(archivePath(root))
	wantRow := "| XR-005 | Задача в работе | task | P2 | 2026-07-08 | [tasks/archive/2026/XR-005.md](tasks/archive/2026/XR-005.md), `deadbee` |\n"
	if !strings.HasSuffix(string(arch), wantRow) {
		t.Fatalf("хвост архива: %s", arch)
	}
}

func TestCloseWithoutFileKeepsLink(t *testing.T) {
	root := setup(t)
	if _, err := cmdClose(root, CloseParams{ID: "XR-001", Date: "2026-07-08"}); err != nil {
		t.Fatal(err)
	}
	arch, _ := os.ReadFile(archivePath(root))
	if !strings.HasSuffix(string(arch), "| XR-001 | Средняя | task/LLD | P2 | 2026-07-08 | (LLD позже) |\n") {
		t.Fatalf("хвост архива: %s", arch)
	}
}

func TestSort(t *testing.T) {
	root := setup(t)
	// Перемешиваем Backlog: хвост наверх, пару с равным R меняем местами.
	b, err := LoadBoard(boardPath(root))
	if err != nil {
		t.Fatal(err)
	}
	rows := b.Sects[SectBacklog].Rows
	lines := []string{b.Lines[rows[3].LineIdx], b.Lines[rows[2].LineIdx], b.Lines[rows[1].LineIdx], b.Lines[rows[0].LineIdx]}
	for i, r := range rows {
		b.Lines[r.LineIdx] = lines[i]
	}
	if err := b.Save(); err != nil {
		t.Fatal(err)
	}
	if _, err := cmdSort(root); err != nil {
		t.Fatal(err)
	}
	got := strings.Join(backlogIDs(t, root), " ")
	if want := "XR-002 XR-001 XR-003 XR-004"; got != want {
		t.Fatalf("после sort: %s, ожидал %s", got, want)
	}
	msg, err := cmdSort(root)
	if err != nil || msg != "Backlog уже отсортирован" {
		t.Fatalf("повторный sort: %q, %v", msg, err)
	}
}

func TestLintClean(t *testing.T) {
	root := setup(t)
	finds, err := cmdLint(root)
	if err != nil {
		t.Fatal(err)
	}
	if len(finds) != 0 {
		t.Fatalf("на чистой доске находки: %v", finds)
	}
}

func TestLintFindings(t *testing.T) {
	root := setup(t)
	board := `# Тест (префикс XR)

## In progress

| ID | Задача | Тип | P | R | Ссылка |
|--------|--------|-----|---|---|--------|

## Check

| ID | Задача | Тип | P | R | Ссылка |
|--------|--------|-----|---|---|--------|

## Backlog

| ID | Задача | Тип | P | R | Ссылка |
|--------|--------|-----|---|---|--------|
| XR-010 | Не тот бакет | task | P1 | 9 (0+4+1+0+4) | x |
| XR-011 | Битая ссылка | task | P3 | 9 (0+4+1+0+4) | [tasks/XR-404.md](tasks/XR-404.md) |
| XR-012 | Стоит ниже старшего | task | P3 | 20 (0+10+5+0+5) | x |
| XR-007 | Дубль с архивом | bug | P2 | 30 (25+0+0+5+0) | x |

## Blocked

Нет.
`
	if err := os.WriteFile(boardPath(root), []byte(board), 0o644); err != nil {
		t.Fatal(err)
	}
	finds, err := cmdLint(root)
	if err != nil {
		t.Fatal(err)
	}
	joined := strings.Join(finds, "\n")
	for _, want := range []string{"дубль ID XR-007", "P=P1, а по R=9", "битая ссылка", "не отсортирован"} {
		if !strings.Contains(joined, want) {
			t.Errorf("нет находки %q среди:\n%s", want, joined)
		}
	}
}

// Полный цикл: завёл, взял в работу, закрыл. Доска возвращается к исходным
// байтам, задача остаётся только в архиве.
func TestCycle(t *testing.T) {
	root := setup(t)
	if _, err := cmdAdd(root, AddParams{Title: "Временная", Type: "task", Rank: "0+1+1+0+1", Link: "x"}); err != nil {
		t.Fatal(err)
	}
	if _, err := cmdMove(root, "XR-008", SectInProgress, ""); err != nil {
		t.Fatal(err)
	}
	if _, err := cmdClose(root, CloseParams{ID: "XR-008", Date: "2026-07-08"}); err != nil {
		t.Fatal(err)
	}
	board, _ := os.ReadFile(boardPath(root))
	if string(board) != fixtureBoard {
		t.Fatalf("доска после цикла не вернулась к исходной:\n%s", board)
	}
	arch, _ := os.ReadFile(archivePath(root))
	if !strings.HasSuffix(string(arch), "| XR-008 | Временная | task | P3 | 2026-07-08 | x |\n") {
		t.Fatalf("хвост архива: %s", arch)
	}
}
