<?php
// Viherd — rendu d'un dump reçu de Symfony VarDumper (ServerDumper).
// Entrée (stdin) : une ligne base64(serialize([Data $data, array $context])).
// Sortie (stdout) : JSON { context, root, html, text }.
// On lit d'abord le contexte sans charger de classe, on retrouve le vendor/autoload.php du projet
// à partir du fichier source, puis on désérialise vraiment pour produire le HTML dépliable.

error_reporting(E_ALL & ~E_DEPRECATED & ~E_USER_DEPRECATED);
$raw = base64_decode(trim(stream_get_contents(STDIN)), true);
if ($raw === false) {
    echo json_encode(['error' => 'payload invalide']);
    exit;
}

$probe = @unserialize($raw, ['allowed_classes' => false]);
$ctx = (is_array($probe) && isset($probe[1]) && is_array($probe[1])) ? $probe[1] : [];
$file = $ctx['source']['file'] ?? null;
if (!$file && isset($ctx['cli']['command_line'])) {
    $file = getcwd();
}

$root = null;
for ($d = $file ? dirname($file) : null; $d && $d !== dirname($d); $d = dirname($d)) {
    if (is_file($d . '/vendor/autoload.php')) {
        $root = $d;
        break;
    }
}

$out = ['context' => $ctx, 'root' => $root];
if ($root) {
    require $root . '/vendor/autoload.php';
    $v = @unserialize($raw);
    if (is_array($v) && isset($v[0]) && $v[0] instanceof \Symfony\Component\VarDumper\Cloner\Data) {
        $html = new \Symfony\Component\VarDumper\Dumper\HtmlDumper();
        if (method_exists($html, 'setTheme')) {
            $html->setTheme(getenv('VIHERD_DUMP_THEME') ?: 'dark');
        }
        $html->setDisplayOptions(['maxDepth' => 2, 'maxStringLength' => 400]);
        $out['html'] = $html->dump($v[0], true);
        $cli = new \Symfony\Component\VarDumper\Dumper\CliDumper();
        $cli->setColors(false);
        $out['text'] = $cli->dump($v[0], true);
    }
}
echo json_encode($out, JSON_INVALID_UTF8_SUBSTITUTE | JSON_PARTIAL_OUTPUT_ON_ERROR | JSON_UNESCAPED_SLASHES);
